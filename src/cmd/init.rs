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
    println!("Signal requires a phone number that can receive SMS.");
    println!("You'll need to verify it with a code sent via text message.");
    println!();

    // Download dependencies if needed
    if setup::find_java().is_none() || setup::find_signal_cli().is_none() {
        print!("Setting up Signal runtime... ");
        std::io::Write::flush(&mut std::io::stdout())?;
        setup::ensure_java().await?;
        setup::ensure_signal_cli().await?;
        println!("done");
    }

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
    println!("(You may need to complete a CAPTCHA in your browser)");
    println!();

    // Register
    if let Err(e) = signal::register_account(&phone_number).await {
        // Registration might fail if already registered, which is fine
        println!("Note: {}", e);
        println!("If already registered, you can proceed with verification.");
    }

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
