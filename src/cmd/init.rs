use anyhow::{Result, bail};
use dialoguer::{Input, Password, Select, theme::ColorfulTheme};
use tracing::info;

use crate::channels::{self, signal, telegram};
use crate::config::{self, Config, SignalConfig, TelegramConfig};
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

        if !configured.is_empty() || config.is_claude_configured() {
            let mut status = Vec::new();
            if !configured.is_empty() {
                status.push(format!("Channels: {}", configured.join(", ")));
            }
            if config.is_claude_configured() {
                status.push("Claude: configured".to_string());
            }
            println!("Current setup: {}", status.join(", "));
            println!();

            let choices = vec![
                "Add/configure a channel",
                "Configure Claude",
                "Reconfigure from scratch",
                "Cancel",
            ];
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("What would you like to do?")
                .items(&choices)
                .default(0)
                .interact()?;

            match selection {
                0 => {
                    add_channel(Some(config)).await?;
                    return Ok(());
                }
                1 => return setup_claude(Some(config)).await,
                2 => {} // Continue to fresh setup
                _ => {
                    println!("Cancelled.");
                    return Ok(());
                }
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

    // Step 2: Claude
    setup_claude(Some(config)).await?;

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
    config.channels.telegram = Some(TelegramConfig { bot_token: token });
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
    config.channels.signal = Some(SignalConfig {
        phone_number: phone_number.clone(),
    });
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
    config.channels.signal = Some(SignalConfig {
        phone_number: phone_number.to_string(),
    });
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
    config.channels.signal = Some(SignalConfig {
        phone_number: phone_number.clone(),
    });
    config.save()?;

    println!();
    println!("Signal linked successfully for {}", phone_number);

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

    // Get authentication
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
            // OAuth / setup-token flow
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
            // API Key flow
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

    // Trim whitespace and normalize
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

    // Save config
    let mut config = existing_config.unwrap_or_default();
    config.claude.api_key = Some(credential);
    config.save()?;

    let paths = config::paths()?;

    println!();
    println!("Setup complete!");
    println!();
    println!("Config saved to: {}", paths.config_file.display());
    println!();
    println!("Run `cica` to start your assistant.");

    info!("Claude setup complete");
    Ok(())
}
