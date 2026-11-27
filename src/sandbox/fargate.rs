//! Fargate launcher (feature `fargate`).
//!
//! Implements `Launcher` via `ecs:RunTask` + a `DescribeTasks` poll, reusing
//! the store-mediated job/result protocol (the `TurnJob` is already in S3; only
//! the small `turn_id` rides the launch as a container command override). AWS
//! calls sit behind the `EcsClient` trait so the launch/poll/stop state machine
//! is testable without AWS; `aws-sdk-ecs` lives only in `AwsEcsClient`.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::sync::OnceCell;
use tokio::time::{Duration, Instant, sleep};

use crate::config::FargateConfig;
use crate::sandbox::worker::{Handle, Launcher, LauncherKind, Status, StopOutcome, WorkerSpec};

/// A request to start one worker task. Pure data, built from config + turn id.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunTaskRequest {
    pub cluster: String,
    pub task_definition: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub assign_public_ip: bool,
    pub container_name: String,
    pub command: Vec<String>,
    pub client_token: Option<String>,
    pub started_by: Option<String>,
}

/// A task's observed status (subset DescribeTasks gives us).
#[derive(Debug, Clone)]
pub(crate) struct TaskStatus {
    pub last_status: String,
    pub exit_code: Option<i32>,
    pub stopped_reason: Option<String>,
}

/// What `FargateLauncher` needs from ECS — the seam keeping aws-sdk-ecs at the edge.
#[async_trait]
pub(crate) trait EcsClient: Send + Sync {
    /// Start a task; returns its ARN. Errors if RunTask reports failures.
    async fn run_task(&self, req: &RunTaskRequest) -> Result<String>;
    /// Current status of a task. `container_name` selects the worker container
    /// so a sidecar can't mask its exit code.
    async fn describe_task(
        &self,
        cluster: &str,
        task_arn: &str,
        container_name: &str,
    ) -> Result<Option<TaskStatus>>;
    /// Best-effort stop (on timeout). The caller logs errors; not fatal.
    async fn stop_task(&self, cluster: &str, task_arn: &str, reason: &str) -> Result<()>;
    /// Lists task ARNs started with an idempotency token.
    async fn list_tasks(&self, cluster: &str, started_by: &str) -> Result<Vec<String>>;
}

/// Real `EcsClient` over aws-sdk-ecs, with a lazily-built client.
pub(crate) struct AwsEcsClient {
    region: Option<String>,
    client: OnceCell<aws_sdk_ecs::Client>,
}

impl AwsEcsClient {
    pub(crate) fn new(region: Option<String>) -> Self {
        Self {
            region,
            client: OnceCell::new(),
        }
    }

    async fn client(&self) -> Result<&aws_sdk_ecs::Client> {
        self.client
            .get_or_try_init(|| async {
                let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
                if let Some(r) = &self.region {
                    loader = loader.region(aws_config::Region::new(r.clone()));
                }
                let shared = loader.load().await;
                Ok::<_, anyhow::Error>(aws_sdk_ecs::Client::new(&shared))
            })
            .await
    }
}

#[async_trait]
impl EcsClient for AwsEcsClient {
    async fn run_task(&self, req: &RunTaskRequest) -> Result<String> {
        use aws_sdk_ecs::types::{
            AssignPublicIp, AwsVpcConfiguration, ContainerOverride, LaunchType,
            NetworkConfiguration, TaskOverride,
        };

        let assign = if req.assign_public_ip {
            AssignPublicIp::Enabled
        } else {
            AssignPublicIp::Disabled
        };
        let vpc = AwsVpcConfiguration::builder()
            .set_subnets(Some(req.subnets.clone()))
            .set_security_groups(Some(req.security_groups.clone()))
            .assign_public_ip(assign)
            .build()
            .context("building awsvpc configuration")?;
        let net = NetworkConfiguration::builder()
            .awsvpc_configuration(vpc)
            .build();
        let overrides = TaskOverride::builder()
            .container_overrides(
                ContainerOverride::builder()
                    .name(req.container_name.clone())
                    .set_command(Some(req.command.clone()))
                    .build(),
            )
            .build();

        let resp = self
            .client()
            .await?
            .run_task()
            .cluster(&req.cluster)
            .task_definition(&req.task_definition)
            .launch_type(LaunchType::Fargate)
            .network_configuration(net)
            .overrides(overrides)
            .set_client_token(req.client_token.clone())
            .set_started_by(req.started_by.clone())
            .send()
            .await
            .context("ecs run_task")?;

        if let Some(f) = resp.failures().first() {
            bail!(
                "ecs run_task failure: arn={:?} reason={:?} detail={:?}",
                f.arn(),
                f.reason(),
                f.detail()
            );
        }
        resp.tasks()
            .first()
            .and_then(|t| t.task_arn())
            .map(|s| s.to_string())
            .context("ecs run_task returned no task arn")
    }

    async fn describe_task(
        &self,
        cluster: &str,
        task_arn: &str,
        container_name: &str,
    ) -> Result<Option<TaskStatus>> {
        let resp = self
            .client()
            .await?
            .describe_tasks()
            .cluster(cluster)
            .tasks(task_arn)
            .send()
            .await
            .context("ecs describe_tasks")?;
        let Some(task) = resp.tasks().first() else {
            return Ok(None);
        };
        let exit_code = task
            .containers()
            .iter()
            .find(|c| c.name() == Some(container_name))
            .and_then(|c| c.exit_code());
        Ok(Some(TaskStatus {
            last_status: task.last_status().unwrap_or("UNKNOWN").to_string(),
            exit_code,
            stopped_reason: task.stopped_reason().map(|s| s.to_string()),
        }))
    }

    async fn stop_task(&self, cluster: &str, task_arn: &str, reason: &str) -> Result<()> {
        self.client()
            .await?
            .stop_task()
            .cluster(cluster)
            .task(task_arn)
            .reason(reason)
            .send()
            .await
            .context("ecs stop_task")?;
        Ok(())
    }

    async fn list_tasks(&self, cluster: &str, started_by: &str) -> Result<Vec<String>> {
        let response = self
            .client()
            .await?
            .list_tasks()
            .cluster(cluster)
            .started_by(started_by)
            .send()
            .await
            .context("ecs list_tasks")?;
        Ok(response
            .task_arns()
            .iter()
            .map(ToString::to_string)
            .collect())
    }
}

/// Launches a worker turn as a one-shot Fargate task.
pub struct FargateLauncher {
    ecs: Box<dyn EcsClient>,
    config: FargateConfig,
}

impl FargateLauncher {
    /// Build the AWS-backed launcher (lazy ECS client).
    pub fn new(config: FargateConfig) -> Self {
        let region = config.region.clone();
        Self::with_client(Box::new(AwsEcsClient::new(region)), config)
    }

    /// Construct with an explicit `EcsClient` (used by `new` and by tests).
    pub(crate) fn with_client(ecs: Box<dyn EcsClient>, config: FargateConfig) -> Self {
        Self { ecs, config }
    }

    fn worker_task_request(&self, spec: &WorkerSpec) -> RunTaskRequest {
        RunTaskRequest {
            cluster: self.config.cluster.clone(),
            task_definition: self.config.task_definition.clone(),
            subnets: self.config.subnets.clone(),
            security_groups: self.config.security_groups.clone(),
            assign_public_ip: self.config.assign_public_ip,
            container_name: self.config.container_name.clone(),
            command: spec.args(),
            client_token: Some(spec.launch_token.clone()),
            started_by: Some(spec.launch_token.clone()),
        }
    }

    async fn describe(&self, arn: &str) -> Result<Option<TaskStatus>> {
        self.ecs
            .describe_task(&self.config.cluster, arn, &self.config.container_name)
            .await
    }
}

#[async_trait]
impl Launcher for FargateLauncher {
    async fn start(&self, spec: &WorkerSpec) -> Result<Handle> {
        let arn = self.ecs.run_task(&self.worker_task_request(spec)).await?;
        let deadline = Instant::now() + spec.start_timeout;
        let mut interval = Duration::from_secs(self.config.poll_interval_secs);
        loop {
            if Instant::now() >= deadline {
                let _ = self
                    .ecs
                    .stop_task(&self.config.cluster, &arn, "cica worker startup timeout")
                    .await;
                bail!(
                    "worker task not running after {}s",
                    spec.start_timeout.as_secs()
                );
            }
            match self.describe(&arn).await? {
                Some(task) if task.last_status == "RUNNING" => {
                    return Ok(Handle {
                        kind: LauncherKind::Fargate,
                        id: arn,
                    });
                }
                Some(task) if task.last_status == "STOPPED" => bail!(
                    "worker task stopped during startup (exit {:?}, reason {:?})",
                    task.exit_code,
                    task.stopped_reason
                ),
                // ECS is eventually consistent: a task RunTask just returned can briefly
                // describe as MISSING.
                _ => {}
            }
            sleep(interval.min(deadline.saturating_duration_since(Instant::now()))).await;
            interval = (interval.saturating_mul(2)).min(Duration::from_secs(30));
        }
    }

    async fn status(&self, handle: &Handle) -> Result<Status> {
        Ok(match self.describe(&handle.id).await? {
            None => Status::NotFound,
            Some(task) if task.last_status == "RUNNING" => Status::Running,
            Some(task) if task.last_status == "STOPPED" => Status::Stopped,
            Some(_) => Status::Unknown,
        })
    }

    async fn stop_and_wait(&self, handle: &Handle, deadline: Duration) -> Result<StopOutcome> {
        if self.describe(&handle.id).await?.is_none() {
            return Ok(StopOutcome::NotFound);
        }
        self.ecs
            .stop_task(&self.config.cluster, &handle.id, "cica worker shutdown")
            .await?;
        let until = Instant::now() + deadline;
        let mut interval = Duration::from_secs(self.config.poll_interval_secs);
        loop {
            match self.describe(&handle.id).await? {
                None => return Ok(StopOutcome::NotFound),
                Some(task) if task.last_status == "STOPPED" => {
                    return Ok(StopOutcome::Terminated);
                }
                _ if Instant::now() >= until => return Ok(StopOutcome::Unknown),
                _ => sleep(interval).await,
            }
            interval = (interval.saturating_mul(2)).min(Duration::from_secs(30));
        }
    }

    async fn reconcile(&self, spec: &WorkerSpec) -> Result<Option<Handle>> {
        Ok(self
            .ecs
            .list_tasks(&self.config.cluster, &spec.launch_token)
            .await?
            .into_iter()
            .next()
            .map(|id| Handle {
                kind: LauncherKind::Fargate,
                id,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn cfg() -> FargateConfig {
        FargateConfig {
            cluster: "cica".into(),
            task_definition: "cica-worker".into(),
            subnets: vec!["subnet-a".into()],
            security_groups: vec!["sg-1".into()],
            assign_public_ip: false,
            region: None,
            container_name: "cica-worker".into(),
            poll_interval_secs: 0, // no real waiting in tests
        }
    }

    struct FakeEcs {
        statuses: Mutex<VecDeque<TaskStatus>>,
        missing: Mutex<usize>,
        run_ok: bool,
        stop_called: AtomicBool,
        requests: Mutex<Vec<RunTaskRequest>>,
        listed: Vec<String>,
    }
    impl FakeEcs {
        fn new(statuses: Vec<TaskStatus>) -> Self {
            Self {
                statuses: Mutex::new(statuses.into()),
                missing: Mutex::new(0),
                run_ok: true,
                stop_called: AtomicBool::new(false),
                requests: Mutex::new(Vec::new()),
                listed: Vec::new(),
            }
        }
    }

    struct ArcEcs(Arc<FakeEcs>);

    #[async_trait]
    impl EcsClient for ArcEcs {
        async fn run_task(&self, request: &RunTaskRequest) -> Result<String> {
            self.0.run_task(request).await
        }

        async fn describe_task(
            &self,
            cluster: &str,
            arn: &str,
            container: &str,
        ) -> Result<Option<TaskStatus>> {
            self.0.describe_task(cluster, arn, container).await
        }

        async fn stop_task(&self, cluster: &str, arn: &str, reason: &str) -> Result<()> {
            self.0.stop_task(cluster, arn, reason).await
        }

        async fn list_tasks(&self, cluster: &str, started_by: &str) -> Result<Vec<String>> {
            self.0.list_tasks(cluster, started_by).await
        }
    }

    #[async_trait]
    impl EcsClient for FakeEcs {
        async fn run_task(&self, req: &RunTaskRequest) -> Result<String> {
            self.requests.lock().unwrap().push(req.clone());
            if self.run_ok {
                Ok("arn:task/1".into())
            } else {
                bail!("run_task failed")
            }
        }
        async fn describe_task(
            &self,
            _c: &str,
            _a: &str,
            _container: &str,
        ) -> Result<Option<TaskStatus>> {
            let mut missing = self.missing.lock().unwrap();
            if *missing > 0 {
                *missing -= 1;
                return Ok(None);
            }
            let mut q = self.statuses.lock().unwrap();
            if q.len() > 1 {
                Ok(q.pop_front())
            } else {
                Ok(q.front().cloned())
            }
        }
        async fn stop_task(&self, _c: &str, _a: &str, _r: &str) -> Result<()> {
            self.stop_called.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn list_tasks(&self, _c: &str, _started_by: &str) -> Result<Vec<String>> {
            Ok(self.listed.clone())
        }
    }

    fn status(last: &str, exit: Option<i32>) -> TaskStatus {
        TaskStatus {
            last_status: last.into(),
            exit_code: exit,
            stopped_reason: None,
        }
    }

    fn spec() -> WorkerSpec {
        WorkerSpec {
            session: "session".into(),
            worker_id: "worker".into(),
            launch_token: "launch-token".into(),
            idle: Duration::from_secs(600),
            turn_timeout: Duration::from_secs(900),
            start_timeout: Duration::from_secs(180),
            policy_hash: "policy".into(),
        }
    }

    #[test]
    fn worker_run_task_request_carries_client_token() {
        let launcher = FargateLauncher::with_client(Box::new(FakeEcs::new(vec![])), cfg());
        let request = launcher.worker_task_request(&spec());
        assert_eq!(request.client_token.as_deref(), Some("launch-token"));
        assert_eq!(request.started_by.as_deref(), Some("launch-token"));
        assert_eq!(request.command, spec().args());
    }

    #[tokio::test]
    async fn start_waits_for_running() {
        let fake = FakeEcs::new(vec![status("PENDING", None), status("RUNNING", None)]);
        let launcher = FargateLauncher::with_client(Box::new(fake), cfg());
        let handle = launcher.start(&spec()).await.unwrap();
        assert_eq!(handle.kind, LauncherKind::Fargate);
        assert_eq!(handle.id, "arn:task/1");
    }

    #[tokio::test]
    async fn start_keeps_polling_while_task_is_missing() {
        let fake = FakeEcs::new(vec![status("RUNNING", None)]);
        *fake.missing.lock().unwrap() = 2;
        let launcher = FargateLauncher::with_client(Box::new(fake), cfg());
        let handle = launcher.start(&spec()).await.unwrap();
        assert_eq!(handle.id, "arn:task/1");
    }

    #[tokio::test]
    async fn start_rejects_boot_failure() {
        let mut stopped = status("STOPPED", Some(1));
        stopped.stopped_reason = Some("image pull failed".into());
        let launcher = FargateLauncher::with_client(Box::new(FakeEcs::new(vec![stopped])), cfg());
        assert!(launcher.start(&spec()).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn start_stops_task_after_timeout() {
        let fake = Arc::new(FakeEcs::new(vec![status("PENDING", None)]));
        let mut config = cfg();
        config.poll_interval_secs = 1;
        let launcher = FargateLauncher::with_client(Box::new(ArcEcs(fake.clone())), config);
        let mut worker = spec();
        worker.start_timeout = Duration::from_secs(180);

        let error = launcher.start(&worker).await.unwrap_err();

        assert_eq!(error.to_string(), "worker task not running after 180s");
        assert!(fake.stop_called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn status_maps_ecs_states() {
        for (task, expected) in [
            (Some(status("RUNNING", None)), Status::Running),
            (Some(status("STOPPED", Some(0))), Status::Stopped),
            (Some(status("PENDING", None)), Status::Unknown),
            (None, Status::NotFound),
        ] {
            let fake = FakeEcs::new(task.into_iter().collect());
            let launcher = FargateLauncher::with_client(Box::new(fake), cfg());
            let handle = Handle {
                kind: LauncherKind::Fargate,
                id: "arn:task/1".into(),
            };
            assert_eq!(launcher.status(&handle).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn stop_and_wait_reports_terminal_and_unknown() {
        let handle = Handle {
            kind: LauncherKind::Fargate,
            id: "arn:task/1".into(),
        };
        let terminal = FargateLauncher::with_client(
            Box::new(FakeEcs::new(vec![
                status("RUNNING", None),
                status("STOPPED", Some(0)),
            ])),
            cfg(),
        );
        assert_eq!(
            terminal
                .stop_and_wait(&handle, Duration::from_secs(1))
                .await
                .unwrap(),
            StopOutcome::Terminated
        );

        let unknown = FargateLauncher::with_client(
            Box::new(FakeEcs::new(vec![status("RUNNING", None)])),
            cfg(),
        );
        assert_eq!(
            unknown
                .stop_and_wait(&handle, Duration::ZERO)
                .await
                .unwrap(),
            StopOutcome::Unknown
        );
    }

    #[tokio::test]
    async fn reconcile_returns_first_matching_task_or_none() {
        let mut found = FakeEcs::new(vec![]);
        found.listed = vec!["arn:task/found".into()];
        let launcher = FargateLauncher::with_client(Box::new(found), cfg());
        assert_eq!(
            launcher.reconcile(&spec()).await.unwrap().unwrap().id,
            "arn:task/found"
        );

        let launcher = FargateLauncher::with_client(Box::new(FakeEcs::new(vec![])), cfg());
        assert!(launcher.reconcile(&spec()).await.unwrap().is_none());
    }
}
