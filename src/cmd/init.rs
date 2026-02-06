use anyhow::{Result, bail};
use dialoguer::{Input, Password, Select, theme::ColorfulTheme};
use tracing::info;

use crate::backends::{claude, cursor};
use crate::channels::{self, signal, slack, telegram};
use crate::config::{self, AiBackend, Config, SignalConfig, SlackConfig, TelegramConfig};
use crate::setup;

/// Run the init command
pub async fn run() -> Result<()> {
    let paths = config::paths()?;

    println!();
    println!("Welcome to Cica!");
    println!();

    // Check if already configured
    if paths.config_file.exists() {
        let config = Config::load()?;
        let configured = config.configured_channels();

        if !configured.is_empty() || config.is_claude_configured() || config.is_cursor_configured()
        {
            let mut status = Vec::new();
            if !configured.is_empty() {
                status.push(format!("Channels: {}", configured.join(", ")));
            }
            let backend_name = match config.backend {
                AiBackend::Claude => "Claude Code",
                AiBackend::Cursor => "Cursor CLI",
            };
            if config.is_backend_configured() {
                status.push(format!("AI Backend: {} (configured)", backend_name));
            } else {
                status.push(format!("AI Backend: {} (not configured)", backend_name));
            }
            println!("Current setup: {}", status.join(", "));
            println!();

            let mut choices = vec![
                "Add/configure a channel",
                "Configure AI backend (Claude Code or Cursor CLI)",
            ];

            let can_switch = config.is_claude_configured() && config.is_cursor_configured();
            if can_switch {
                choices.push("Switch active AI backend");
            }

            choices.push("Reconfigure from scratch");
            choices.push("Cancel");

            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("What would you like to do?")
                .items(&choices)
                .default(0)
                .interact()?;

            let selected = choices[selection];
            if selected == "Add/configure a channel" {
                add_channel(Some(config)).await?;
                return Ok(());
            } else if selected == "Configure AI backend (Claude Code or Cursor CLI)" {
                return setup_ai_backend(Some(config)).await;
            } else if selected == "Switch active AI backend" {
                return switch_ai_backend(config).await;
            } else if selected == "Reconfigure from scratch" {
                // fall through to fresh setup
            } else {
                println!("Cancelled.");
                return Ok(());
            }
        }
    }

    // Fresh setup
    paths.ensure_dirs()?;
    full_setup().await
}

/// Full setup wizard for first-time users
async fn full_setup() -> Result<()> {
    // Step 1: Channel
    let config = add_channel(None).await?;

    // Step 2: AI Backend
    setup_ai_backend(Some(config)).await?;

    Ok(())
}

/// Set up AI backend (Claude Code or Cursor CLI)
async fn setup_ai_backend(existing_config: Option<Config>) -> Result<()> {
    println!();
    println!("AI Backend Setup");
    println!("────────────────");
    println!();

    let has_backend = existing_config
        .as_ref()
        .is_some_and(|c| c.is_backend_configured());

    if has_backend {
        let config = existing_config.as_ref().unwrap();
        let backend_name = match config.backend {
            AiBackend::Claude => "Claude Code",
            AiBackend::Cursor => "Cursor CLI",
        };
        let current_model = match config.backend {
            AiBackend::Claude => config.claude.model.as_deref(),
            AiBackend::Cursor => config.cursor.model.as_deref(),
        };
        println!(
            "Current: {} (model: {})",
            backend_name,
            current_model.unwrap_or("default")
        );
        println!();

        let choices = vec![
            "Change model",
            "Reconfigure backend (Claude Code or Cursor CLI)",
            "Cancel",
        ];

        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("What would you like to do?")
            .items(&choices)
            .default(0)
            .interact()?;

        return match selection {
            0 => change_model(existing_config.unwrap()).await,
            1 => pick_backend(existing_config).await,
            _ => {
                println!("Cancelled.");
                Ok(())
            }
        };
    }

    pick_backend(existing_config).await
}

async fn pick_backend(existing_config: Option<Config>) -> Result<()> {
    println!("Cica can use either Claude Code or Cursor CLI as its AI backend.");
    println!();

    let choices = vec![
        "Claude Code   Anthropic's official CLI (recommended)",
        "Cursor CLI    Multi-model support (Claude, GPT, Gemini)",
    ];

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Which AI backend would you like to use?")
        .items(&choices)
        .default(0)
        .interact()?;

    match selection {
        0 => setup_claude(existing_config).await,
        1 => setup_cursor(existing_config).await,
        _ => unreachable!(),
    }
}

/// Change the model for the active backend
async fn change_model(mut config: Config) -> Result<()> {
    let (backend_name, current_model) = match config.backend {
        AiBackend::Claude => ("Claude Code", config.claude.model.as_deref()),
        AiBackend::Cursor => ("Cursor CLI", config.cursor.model.as_deref()),
    };

    println!();
    println!("Change Model");
    println!("────────────");
    println!();
    println!(
        "Backend: {} | Current model: {}",
        backend_name,
        current_model.unwrap_or("default")
    );
    println!();

    let new_model = match config.backend {
        AiBackend::Claude => select_model(backend_name, claude::MODELS, current_model)?,
        AiBackend::Cursor => {
            let api_key = config
                .cursor
                .api_key
                .as_deref()
                .unwrap_or_default()
                .to_string();
            print!("Fetching available models... ");
            std::io::Write::flush(&mut std::io::stdout())?;
            let models = cursor::list_models(&api_key).await;
            println!("OK ({} models)", models.len());
            println!();
            select_model(backend_name, &models, current_model)?
        }
    };

    match config.backend {
        AiBackend::Claude => config.claude.model = new_model.clone(),
        AiBackend::Cursor => config.cursor.model = new_model.clone(),
    }

    config.save()?;

    println!();
    println!(
        "Model set to: {}",
        new_model.as_deref().unwrap_or("default")
    );

    Ok(())
}

/// Switch between configured AI backends
async fn switch_ai_backend(mut config: Config) -> Result<()> {
    println!();
    println!("Switch AI Backend");
    println!("─────────────────");
    println!();

    let current = match config.backend {
        AiBackend::Claude => "Claude Code",
        AiBackend::Cursor => "Cursor CLI",
    };
    let other = match config.backend {
        AiBackend::Claude => "Cursor CLI",
        AiBackend::Cursor => "Claude Code",
    };

    println!("Current backend: {}", current);
    println!();

    let choices = vec![format!("Switch to {}", other), "Cancel".to_string()];

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("What would you like to do?")
        .items(&choices)
        .default(0)
        .interact()?;

    if selection == 0 {
        config.backend = match config.backend {
            AiBackend::Claude => AiBackend::Cursor,
            AiBackend::Cursor => AiBackend::Claude,
        };
        config.save()?;

        let new_backend = match config.backend {
            AiBackend::Claude => "Claude Code",
            AiBackend::Cursor => "Cursor CLI",
        };
        println!();
        println!("Switched to {}!", new_backend);
    } else {
        println!("Cancelled.");
    }

    Ok(())
}

/// Add a channel to the configuration
async fn add_channel(existing_config: Option<Config>) -> Result<Config> {
    // For now, only Telegram is supported
    let channel_choices: Vec<&str> = channels::SUPPORTED_CHANNELS
        .iter()
        .map(|c| c.display_name)
        .collect();

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Which channel would you like to set up?")
        .items(&channel_choices)
        .default(0)
        .interact()?;

    let channel = &channels::SUPPORTED_CHANNELS[selection];

    match channel.name {
        "telegram" => setup_telegram(existing_config).await,
        "signal" => setup_signal(existing_config).await,
        "slack" => setup_slack(existing_config).await,
        _ => bail!("Channel not yet supported: {}", channel.name),
    }
}

/// Set up Telegram
async fn setup_telegram(existing_config: Option<Config>) -> Result<Config> {
    println!();
    println!("Telegram Setup");
    println!("──────────────");
    println!();
    println!("1. Open Telegram and message @BotFather");
    println!("2. Send /newbot and follow the prompts");
    println!("3. Copy the bot token you receive");
    println!();

    let token: String = Password::with_theme(&ColorfulTheme::default())
        .with_prompt("Paste your bot token")
        .interact()?;

    print!("Validating... ");

    match telegram::validate_token(&token).await {
        Ok(username) => {
            println!("OK");
            println!("Connected as @{}", username);
        }
        Err(e) => {
            println!("FAILED");
            bail!("Invalid token: {}", e);
        }
    }

    // Build config
    let mut config = existing_config.unwrap_or_default();
    config.channels.telegram = Some(TelegramConfig::new(token));
    config.save()?;

    info!("Telegram setup complete");
    Ok(config)
}

/// Set up Signal
async fn setup_signal(existing_config: Option<Config>) -> Result<Config> {
    println!();
    println!("Signal Setup");
    println!("────────────");
    println!();

    // Download dependencies if needed
    if setup::find_java().is_none() || setup::find_signal_cli().is_none() {
        print!("Setting up Signal runtime... ");
        std::io::Write::flush(&mut std::io::stdout())?;
        setup::ensure_java().await?;
        setup::ensure_signal_cli().await?;
        println!("done");
        println!();
    }

    // Offer choice between linking and registering
    let choices = vec![
        "Link to existing Signal account (if you have Signal on your phone)",
        "Register a new phone number",
    ];

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("How would you like to set up Signal?")
        .items(&choices)
        .default(0)
        .interact()?;

    if selection == 0 {
        return link_signal_device(existing_config).await;
    }

    // Registration flow
    println!();
    println!("Signal requires a phone number that can receive SMS.");
    println!("You'll need to verify it with a code sent via text message.");
    println!();

    // Get phone number
    let phone_number: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Phone number (with country code, e.g., +1234567890)")
        .interact_text()?;

    // Validate format
    if !phone_number.starts_with('+') {
        bail!("Phone number must start with + and country code (e.g., +1 for US)");
    }

    println!();
    println!("Registering with Signal...");

    // Try registration, handle CAPTCHA if needed
    let mut captcha: Option<String> = None;
    let mut use_voice = false;
    loop {
        match signal::register_account(&phone_number, captcha.as_deref(), use_voice).await? {
            signal::RegistrationResult::Success => {
                println!("Registration successful! SMS verification code sent.");
                break;
            }
            signal::RegistrationResult::AlreadyRegistered => {
                println!("Phone number is already registered with signal-cli.");
                break;
            }
            signal::RegistrationResult::RateLimited => {
                println!();
                println!("Rate limited by Signal - too many registration attempts.");
                println!("You'll need to wait before trying again (usually 12-24 hours).");
                println!();
                println!("In the meantime, you can:");
                println!("  - Link as a secondary device if you have Signal on your phone");
                println!("  - Try with a different phone number");
                println!();

                let choices = vec![
                    "Link as secondary device",
                    "Use a different phone number",
                    "Cancel (try again later)",
                ];
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("What would you like to do?")
                    .items(&choices)
                    .default(0)
                    .interact()?;

                match selection {
                    0 => return link_signal_device(existing_config).await,
                    1 => {
                        let new_phone: String = Input::with_theme(&ColorfulTheme::default())
                            .with_prompt("Phone number (with country code)")
                            .interact_text()?;
                        if !new_phone.starts_with('+') {
                            bail!("Phone number must start with + and country code");
                        }
                        return setup_signal_with_number(existing_config, &new_phone).await;
                    }
                    _ => {
                        println!("Cancelled. Try again later.");
                        return Ok(existing_config.unwrap_or_default());
                    }
                }
            }
            signal::RegistrationResult::AuthorizationFailed => {
                println!();
                if use_voice {
                    // Already tried voice, show other options
                    println!("Authorization failed with voice verification too.");
                    println!("This number may be blocked or unsupported by Signal.");
                } else {
                    // Offer voice verification as an option
                    println!("SMS verification failed (common with some carriers).");
                }
                println!();

                let mut choices = vec![];
                if !use_voice {
                    choices.push("Try voice call verification (phone call with code)");
                }
                choices.push("Link as secondary device (if you have Signal on your phone)");
                choices.push("Use a different phone number");
                choices.push("Cancel");

                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("What would you like to do?")
                    .items(&choices)
                    .default(0)
                    .interact()?;

                let choice = choices[selection];

                if choice.starts_with("Try voice") {
                    println!();
                    println!("Signal requires waiting 60 seconds after SMS attempt...");
                    print!("Waiting: ");
                    std::io::Write::flush(&mut std::io::stdout())?;
                    for i in (1..=60).rev() {
                        print!("\rWaiting: {}s ", i);
                        std::io::Write::flush(&mut std::io::stdout())?;
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }
                    println!("\rRequesting voice call...               ");
                    use_voice = true;
                    // Keep the captcha - voice verification still needs it
                    continue;
                } else if choice.starts_with("Link as") {
                    return link_signal_device(existing_config).await;
                } else if choice.starts_with("Use a different") {
                    let new_phone: String = Input::with_theme(&ColorfulTheme::default())
                        .with_prompt("Phone number (with country code)")
                        .interact_text()?;
                    if !new_phone.starts_with('+') {
                        bail!("Phone number must start with + and country code");
                    }
                    return setup_signal_with_number(existing_config, &new_phone).await;
                } else {
                    println!("Cancelled.");
                    return Ok(existing_config.unwrap_or_default());
                }
            }
            signal::RegistrationResult::CaptchaRequired => {
                println!();
                println!("CAPTCHA required. Please complete the following steps:");
                println!();
                println!("1. Open: https://signalcaptchas.org/registration/generate.html");
                println!("2. Solve the CAPTCHA");
                println!("3. Right-click the \"Open Signal\" link and copy the link address");
                println!("4. Paste the full link below (starts with signalcaptcha://)");
                println!();

                let captcha_input: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Paste the CAPTCHA link")
                    .interact_text()?;

                let token = captcha_input.trim().to_string();

                if token.is_empty() {
                    println!("Empty CAPTCHA token. Please try again.");
                    continue;
                }

                // Debug: show what we're using
                println!("Token starts with: {}...", &token[..token.len().min(60)]);

                captcha = Some(token);
                println!();
                println!("Retrying registration with CAPTCHA...");
            }
        }
    }

    println!();

    // Get verification code
    let code: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Enter the verification code from SMS")
        .interact_text()?;

    // Remove any spaces/dashes from the code
    let code = code.replace([' ', '-'], "");

    print!("Verifying... ");
    std::io::Write::flush(&mut std::io::stdout())?;

    match signal::verify_account(&phone_number, &code).await {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("FAILED");
            bail!("Verification failed: {}", e);
        }
    }

    // Build config
    let mut config = existing_config.unwrap_or_default();
    config.channels.signal = Some(SignalConfig::new(phone_number.clone()));
    config.save()?;

    println!();
    println!("Signal setup complete for {}", phone_number);

    info!("Signal setup complete");
    Ok(config)
}

/// Helper to retry Signal setup with a specific phone number
async fn setup_signal_with_number(
    existing_config: Option<Config>,
    phone_number: &str,
) -> Result<Config> {
    // Validate format
    if !phone_number.starts_with('+') {
        bail!("Phone number must start with + and country code (e.g., +1 for US)");
    }

    println!();
    println!("Registering with Signal...");

    let mut captcha: Option<String> = None;
    let mut use_voice = false;

    loop {
        match signal::register_account(phone_number, captcha.as_deref(), use_voice).await? {
            signal::RegistrationResult::Success => {
                if use_voice {
                    println!("Registration successful! You should receive a voice call shortly.");
                } else {
                    println!("Registration successful! SMS verification code sent.");
                }
                break;
            }
            signal::RegistrationResult::AlreadyRegistered => {
                println!("Phone number is already registered with signal-cli.");
                break;
            }
            signal::RegistrationResult::AuthorizationFailed => {
                if use_voice {
                    bail!("Authorization failed. This number may not be supported by Signal.");
                }
                println!();
                println!("SMS verification failed. Trying voice call...");
                use_voice = true;
                captcha = None;
                continue;
            }
            signal::RegistrationResult::RateLimited => {
                bail!("Rate limited by Signal. Please wait 12-24 hours before trying again.");
            }
            signal::RegistrationResult::CaptchaRequired => {
                println!();
                println!("CAPTCHA required. Please complete the following steps:");
                println!();
                println!("1. Open: https://signalcaptchas.org/registration/generate.html");
                println!("2. Solve the CAPTCHA");
                println!("3. Right-click the \"Open Signal\" link and copy the link address");
                println!("4. Paste the full link below");
                println!();

                let captcha_input: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Paste the CAPTCHA link")
                    .interact_text()?;

                let token = captcha_input.trim().to_string();
                if token.is_empty() {
                    println!("Empty CAPTCHA token. Please try again.");
                    continue;
                }
                captcha = Some(token);
                println!();
                println!("Retrying registration with CAPTCHA...");
            }
        }
    }

    println!();

    let code: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Enter the verification code")
        .interact_text()?;

    let code = code.replace([' ', '-'], "");

    print!("Verifying... ");
    std::io::Write::flush(&mut std::io::stdout())?;

    match signal::verify_account(phone_number, &code).await {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("FAILED");
            bail!("Verification failed: {}", e);
        }
    }

    let mut config = existing_config.unwrap_or_default();
    config.channels.signal = Some(SignalConfig::new(phone_number.to_string()));
    config.save()?;

    println!();
    println!("Signal setup complete for {}", phone_number);

    Ok(config)
}

/// Link signal-cli as a secondary device to an existing Signal account
async fn link_signal_device(existing_config: Option<Config>) -> Result<Config> {
    println!();
    println!("Link as Secondary Device");
    println!("─────────────────────────");
    println!();
    println!("This will link Cica to your existing Signal account,");
    println!("similar to how Signal Desktop works.");
    println!();

    let paths = config::paths()?;
    let java = setup::find_java().ok_or_else(|| anyhow::anyhow!("Java not found"))?;
    let signal_cli =
        setup::find_signal_cli().ok_or_else(|| anyhow::anyhow!("signal-cli not found"))?;

    std::fs::create_dir_all(&paths.signal_data_dir)?;

    let java_home = java
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Could not determine JAVA_HOME"))?;

    use tokio::io::{AsyncBufReadExt, BufReader};

    // Run signal-cli link command and capture output line by line
    let mut child = tokio::process::Command::new(&signal_cli)
        .args([
            "--config",
            paths.signal_data_dir.to_str().unwrap(),
            "link",
            "-n",
            "Cica",
        ])
        .env("JAVA_HOME", java_home)
        .env(
            "PATH",
            format!(
                "{}:{}",
                java.parent().unwrap().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut link_url = None;

    // Read output looking for the link URL
    loop {
        tokio::select! {
            line = stdout_reader.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        if text.starts_with("sgnl://") {
                            link_url = Some(text.clone());
                            println!();
                            println!("Link URL (open on your phone or copy to Signal):");
                            println!();
                            println!("  {}", text);
                            println!();
                            println!("In Signal app: Settings → Linked Devices → Link New Device");
                            println!();
                            println!("Waiting for you to scan...");
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            line = stderr_reader.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        if text.starts_with("sgnl://") {
                            link_url = Some(text.clone());
                            println!();
                            println!("Link URL (open on your phone or copy to Signal):");
                            println!();
                            println!("  {}", text);
                            println!();
                            println!("In Signal app: Settings → Linked Devices → Link New Device");
                            println!();
                            println!("Waiting for you to scan...");
                        } else if text.contains("error") || text.contains("Error") {
                            // Only print actual errors, not debug output
                            if !text.contains("DEBUG") && !text.contains("INFO") {
                                println!("{}", text);
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }
    }

    let status = child.wait().await?;

    if !status.success() {
        if link_url.is_some() {
            println!();
            println!("Link timed out or was cancelled.");
            println!("Please try again and scan the QR code within 60 seconds.");
        }
        bail!("Link command failed");
    }

    println!();
    println!("Link successful!");

    // After linking, we need to find the account number
    // List accounts to get the linked number
    let output = tokio::process::Command::new(&signal_cli)
        .args([
            "--config",
            paths.signal_data_dir.to_str().unwrap(),
            "listAccounts",
        ])
        .env("JAVA_HOME", java_home)
        .env(
            "PATH",
            format!(
                "{}:{}",
                java.parent().unwrap().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Find phone number in output (format: +1234567890)
    let phone_number = stdout
        .lines()
        .find_map(|line| {
            line.split_whitespace()
                .find(|word| word.starts_with('+') && word.len() > 5)
        })
        .map(|s| s.to_string());

    let phone_number = match phone_number {
        Some(num) => num,
        None => {
            // Ask user to provide the number
            Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Enter the phone number of the linked account")
                .interact_text()?
        }
    };

    let mut config = existing_config.unwrap_or_default();
    config.channels.signal = Some(SignalConfig::new(phone_number.clone()));
    config.save()?;

    println!();
    println!("Signal linked successfully for {}", phone_number);

    Ok(config)
}

/// Set up Slack
async fn setup_slack(existing_config: Option<Config>) -> Result<Config> {
    println!();
    println!("Slack Setup");
    println!("───────────");
    println!();
    println!("You'll need a Slack app with Socket Mode enabled.");
    println!();
    println!("If you haven't created one yet:");
    println!("1. Go to https://api.slack.com/apps");
    println!("2. Click 'Create New App' → 'From scratch'");
    println!("3. Name it and select your workspace");
    println!();
    println!("Required setup in your Slack app:");
    println!("─────────────────────────────────");
    println!();
    println!("1. Enable Socket Mode:");
    println!("   Settings → Socket Mode → Enable");
    println!("   Generate an App-Level Token with 'connections:write' scope");
    println!();
    println!("2. Enable App Home messages:");
    println!("   Features → App Home → Show Tabs → Messages Tab: ON");
    println!("   Check: 'Allow users to send Slash commands and messages'");
    println!();
    println!("3. Subscribe to events:");
    println!("   Features → Event Subscriptions → Enable");
    println!("   Subscribe to bot events: message.im");
    println!();
    println!("4. Add OAuth scopes:");
    println!("   Features → OAuth & Permissions → Bot Token Scopes:");
    println!("   - chat:write");
    println!("   - im:history");
    println!("   - im:read");
    println!("   - im:write");
    println!("   - users:read");
    println!();
    println!("5. Install the app to your workspace");
    println!();

    // Get Bot Token
    let bot_token: String = Password::with_theme(&ColorfulTheme::default())
        .with_prompt("Paste your Bot Token (xoxb-...)")
        .interact()?;

    // Get App Token
    let app_token: String = Password::with_theme(&ColorfulTheme::default())
        .with_prompt("Paste your App Token (xapp-...)")
        .interact()?;

    print!("Validating... ");
    std::io::Write::flush(&mut std::io::stdout())?;

    match slack::validate_credentials(&bot_token, &app_token).await {
        Ok(bot_user_id) => {
            println!("OK");
            println!("Connected as bot user: {}", bot_user_id);
        }
        Err(e) => {
            println!("FAILED");
            bail!("Invalid credentials: {}", e);
        }
    }

    // Build config
    let mut config = existing_config.unwrap_or_default();
    config.channels.slack = Some(SlackConfig::new(bot_token, app_token));
    config.save()?;

    info!("Slack setup complete");
    Ok(config)
}

/// Set up Claude (Bun + Claude Code + API key)
async fn setup_claude(existing_config: Option<Config>) -> Result<()> {
    println!();
    println!("Claude Setup");
    println!("────────────");

    // Ensure runtime dependencies are available
    if setup::find_bun().is_none() || setup::find_claude_code().is_none() {
        println!();
        print!("Setting up runtime... ");
        std::io::Write::flush(&mut std::io::stdout())?;

        setup::ensure_bun().await?;
        setup::ensure_claude_code().await?;
        setup::ensure_embedding_model()?;

        println!("done");
    }

    // Check for existing env token first
    if let Some(env_token) = setup::get_env_oauth_token() {
        println!();
        print!("Found OAuth token in environment, validating... ");
        std::io::Write::flush(&mut std::io::stdout())?;

        match setup::validate_credential(&env_token).await {
            Ok(()) => {
                println!("OK");

                // Save config
                let mut config = existing_config.unwrap_or_default();
                config.claude.api_key = Some(env_token);
                config.save()?;

                let paths = config::paths()?;

                println!();
                println!("Setup complete!");
                println!();
                println!("Config saved to: {}", paths.config_file.display());
                println!();
                println!("Run `cica` to start your assistant.");

                info!("Claude setup complete (from env)");
                return Ok(());
            }
            Err(_) => {
                println!("invalid, continuing with manual setup");
            }
        }
    }

    // Choose provider: Anthropic vs Google Vertex AI
    println!();
    println!("Claude Code can use Anthropic directly or Google Vertex AI (GCP).");
    println!();

    let provider_choices = vec![
        "Anthropic   Subscription (Pro/Max/Team) or API key",
        "Google Vertex AI   GCP project (billing via Google Cloud)",
    ];

    let provider_selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select Claude Code provider")
        .items(&provider_choices)
        .default(0)
        .interact()?;

    let mut config = existing_config.unwrap_or_default();
    let was_using_cursor = config.backend == AiBackend::Cursor && config.is_cursor_configured();

    if provider_selection == 1 {
        // Vertex AI setup
        let paths = config::paths()?;
        println!();
        println!("Vertex AI Setup");
        println!("───────────────");
        println!();
        println!("You need a GCP project with Vertex AI enabled and Claude models");
        println!("enabled in Model Garden.");
        println!();

        let project_id: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("GCP project ID")
            .interact_text()?;

        let region: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt(
                "Region (e.g. europe-west1 or us-east5; see code.claude.com/docs/google-vertex-ai)",
            )
            .default("europe-west1".to_string())
            .interact_text()?;

        let auth_choices = vec![
            "Service account key file (JSON)   Long-lived; recommended for servers",
            "gcloud application-default login  Uses your user credentials (may expire)",
        ];
        let auth_selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("GCP auth (for servers use a service account key so auth does not expire)")
            .items(&auth_choices)
            .default(0)
            .interact()?;

        let vertex_credentials_path: Option<String> = if auth_selection == 0 {
            println!();
            println!("Create a service account in GCP with Vertex AI User (or similar),");
            println!("download its JSON key, and enter the path below.");
            println!("Path can be absolute or relative to your Cica config directory.");
            println!();
            let path: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Path to service account JSON key file")
                .interact_text()?;
            let path = path.trim().to_string();
            if path.is_empty() {
                None
            } else {
                print!("Validating key file... ");
                std::io::Write::flush(&mut std::io::stdout())?;
                match setup::validate_vertex_credentials_path(&path, &paths.base) {
                    Ok(()) => {
                        println!("OK");
                        Some(path)
                    }
                    Err(e) => {
                        println!("FAILED");
                        bail!("Invalid credentials file: {}", e);
                    }
                }
            }
        } else {
            None
        };

        print!("Validating Vertex config... ");
        std::io::Write::flush(&mut std::io::stdout())?;

        match setup::validate_vertex_config(
            project_id.trim(),
            Some(region.trim()),
            vertex_credentials_path.as_deref(),
            &paths.base,
        )
        .await
        {
            Ok(()) => println!("OK"),
            Err(e) => {
                println!("FAILED");
                bail!("Vertex AI setup failed: {}", e);
            }
        }

        config.claude.api_key = None;
        config.claude.use_vertex = true;
        config.claude.vertex_project_id = Some(project_id.trim().to_string());
        config.claude.vertex_region = if region.trim().is_empty() {
            None
        } else {
            Some(region.trim().to_string())
        };
        config.claude.vertex_credentials_path = vertex_credentials_path;
    } else {
        // Anthropic setup
        println!();
        println!("Cica uses Claude Code, which can be billed through your Claude");
        println!("subscription or based on API usage through your Console account.");
        println!();

        let auth_choices = vec![
            "Claude subscription   Pro, Max, Team, or Enterprise",
            "Anthropic Console     API usage billing",
        ];

        let auth_selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select login method")
            .items(&auth_choices)
            .default(0)
            .interact()?;

        let credential = match auth_selection {
            0 => {
                println!();
                println!("Run this command in any terminal:");
                println!();
                println!("  claude setup-token");
                println!();
                println!("Note: The token may display across two lines, but it's one");
                println!("continuous string. Copy and paste it as a single line.");
                println!();

                Password::with_theme(&ColorfulTheme::default())
                    .with_prompt("Paste the setup token")
                    .interact()?
            }
            1 => {
                println!();
                println!("1. Go to https://console.anthropic.com/settings/keys");
                println!("2. Create a new API key");
                println!();

                Password::with_theme(&ColorfulTheme::default())
                    .with_prompt("Paste your API key")
                    .interact()?
            }
            _ => unreachable!(),
        };

        let credential = credential.trim().to_string();

        print!("Validating... ");
        std::io::Write::flush(&mut std::io::stdout())?;

        match setup::validate_credential(&credential).await {
            Ok(()) => println!("OK"),
            Err(e) => {
                println!("FAILED");
                bail!("Authentication failed: {}", e);
            }
        }

        config.claude.api_key = Some(credential);
        config.claude.use_vertex = false;
        config.claude.vertex_project_id = None;
        config.claude.vertex_region = None;
        config.claude.vertex_credentials_path = None;
    }

    // Model selection
    println!();
    config.claude.model = select_model(
        "Claude Code",
        claude::MODELS,
        config.claude.model.as_deref(),
    )?;

    // Ask whether to switch if another backend was active
    if was_using_cursor {
        println!();
        let switch = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Switch to Claude Code as your active backend?")
            .items(&["Yes", "No, keep using Cursor CLI"])
            .default(0)
            .interact()?;

        if switch == 0 {
            config.backend = AiBackend::Claude;
        }
    } else {
        config.backend = AiBackend::Claude;
    }

    config.save()?;

    let paths = config::paths()?;
    let active = match config.backend {
        AiBackend::Claude => "Claude Code",
        AiBackend::Cursor => "Cursor CLI",
    };
    let model_display = config.claude.model.as_deref().unwrap_or("default");

    println!();
    println!(
        "Setup complete! Active backend: {} (model: {})",
        active, model_display
    );
    println!();
    println!("Config saved to: {}", paths.config_file.display());
    println!();
    println!("Run `cica` to start your assistant.");

    info!("Claude setup complete");
    Ok(())
}

/// Interactive model picker shared across backends.
fn select_model<S: AsRef<str>>(
    backend_name: &str,
    models: &[(S, S)],
    current: Option<&str>,
) -> Result<Option<String>> {
    println!("Select a model for {}:", backend_name);
    println!();

    let max_id_len = models
        .iter()
        .map(|(id, _)| id.as_ref().len())
        .max()
        .unwrap_or(20);
    let pad = max_id_len.max(20);

    let mut choices: Vec<String> = Vec::with_capacity(models.len() + 2);
    choices.push(format!(
        "{:<pad$} Use the default model",
        "default",
        pad = pad
    ));
    for (id, name) in models {
        choices.push(format!(
            "{:<pad$} {}",
            id.as_ref(),
            name.as_ref(),
            pad = pad
        ));
    }
    choices.push(format!(
        "{:<pad$} Enter a model name manually",
        "custom",
        pad = pad
    ));

    let current_idx = current
        .and_then(|c| {
            models
                .iter()
                .position(|(id, _)| id.as_ref() == c)
                .map(|i| i + 1)
        })
        .unwrap_or(0);

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Which model would you like to use?")
        .items(&choices)
        .default(current_idx)
        .interact()?;

    if selection == 0 {
        Ok(None)
    } else if selection == choices.len() - 1 {
        let custom: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Model name")
            .interact_text()?;
        let trimmed = custom.trim().to_string();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed))
        }
    } else {
        let (id, _) = &models[selection - 1];
        Ok(Some(id.as_ref().to_string()))
    }
}

/// Set up Cursor CLI
async fn setup_cursor(existing_config: Option<Config>) -> Result<()> {
    println!();
    println!("Cursor CLI Setup");
    println!("────────────────");

    // Ensure runtime dependencies are available
    if setup::find_cursor_cli().is_none() || setup::find_bun().is_none() {
        println!();
        print!("Setting up runtime... ");
        std::io::Write::flush(&mut std::io::stdout())?;

        setup::ensure_bun().await?; // Needed for skills
        setup::ensure_cursor_cli().await?;
        setup::ensure_embedding_model()?;

        println!("done");
    }

    // Get API key
    println!();
    println!("Cursor CLI requires an API key for authentication.");
    println!();
    println!("To get your API key:");
    println!("1. Go to https://cursor.com/settings");
    println!("2. Navigate to: Integrations → User API Keys");
    println!("3. Generate a new API key");
    println!();

    let api_key: String = Password::with_theme(&ColorfulTheme::default())
        .with_prompt("Paste your Cursor API key")
        .interact()?;

    let api_key = api_key.trim().to_string();

    print!("Validating... ");
    std::io::Write::flush(&mut std::io::stdout())?;

    match setup::validate_cursor_api_key(&api_key).await {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("FAILED");
            bail!("Invalid API key: {}", e);
        }
    }

    // Model selection
    println!();
    print!("Fetching available models... ");
    std::io::Write::flush(&mut std::io::stdout())?;
    let cursor_models = cursor::list_models(&api_key).await;
    println!("OK ({} models)", cursor_models.len());
    println!();
    let model = select_model("Cursor CLI", &cursor_models, None)?;

    // Save config
    let mut config = existing_config.unwrap_or_default();
    let was_using_claude = config.backend == AiBackend::Claude && config.is_claude_configured();
    config.cursor.api_key = Some(api_key);
    config.cursor.model = model;

    // Ask whether to switch if another backend was active
    if was_using_claude {
        println!();
        let switch = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Switch to Cursor CLI as your active backend?")
            .items(&["Yes", "No, keep using Claude Code"])
            .default(0)
            .interact()?;

        if switch == 0 {
            config.backend = AiBackend::Cursor;
        }
    } else {
        config.backend = AiBackend::Cursor;
    }

    config.save()?;

    let paths = config::paths()?;
    let active = match config.backend {
        AiBackend::Claude => "Claude Code",
        AiBackend::Cursor => "Cursor CLI",
    };

    println!();
    println!("Setup complete! Active backend: {}", active);
    println!();
    println!("Config saved to: {}", paths.config_file.display());
    println!();
    println!("Run `cica` to start your assistant.");

    info!("Cursor CLI setup complete");
    Ok(())
}
