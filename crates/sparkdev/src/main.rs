fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "__return_worker") {
        std::process::exit(if sparkdev::returns::worker().is_ok() { 0 } else { 1 });
    }
    if args.first().is_some_and(|a| a == "__worker") {
        let result = args
            .get(1)
            .ok_or_else(|| sparkdev::AppError::new(2, "Missing worker job"))
            .and_then(|job| sparkdev::local::worker(job));
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    if let Some(client) = sparkdev::mcp_client(&args) {
        let device = sparkdev::mcp_device(&args);
        std::process::exit(if sparkdev::mcp::serve_with_device(&client, device.as_deref()).is_ok() {
            0
        } else {
            3
        });
    }
    let json_mode = sparkdev::json_output(&args);
    match sparkdev::execute_with_events(args, &mut |event| {
        if json_mode {
            println!("{event}");
        } else if let Some(text) = event["text"].as_str() {
            print!("{text}");
        } else {
            eprintln!("{event}");
        }
    }) {
        Ok(value) => {
            println!(
                "{}",
                if json_mode {
                    serde_json::to_string(&value).unwrap()
                } else if value.get("downloads").is_some() && value["message"].is_string() {
                    value["message"].as_str().unwrap().to_owned()
                } else if let Some(file) = value["file"].as_str() {
                    let inline = value
                        .get("json")
                        .map(|v| format!("\n{}", serde_json::to_string_pretty(v).unwrap()))
                        .unwrap_or_default();
                    format!("{file}{inline}")
                } else {
                    serde_json::to_string_pretty(&value).unwrap()
                }
            );
            let code = value
                .get("process_exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            std::process::exit(code.clamp(0, 255) as i32);
        }
        Err(error) => {
            if error.code == 0 {
                println!("{}", error.message);
                return;
            }
            let value = serde_json::json!({"ok":false,"code":error.code,"message":error.message});
            if json_mode {
                println!("{value}");
            } else {
                eprintln!("sparkdev: {}", error.message);
            }
            std::process::exit(error.code as i32);
        }
    }
}
