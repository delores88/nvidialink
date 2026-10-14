fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "__worker") {
        let result = args
            .get(1)
            .ok_or_else(|| sparkdev::AppError::new(2, "Missing worker job"))
            .and_then(|job| sparkdev::local::worker(job));
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    if let Some(client) = sparkdev::mcp_client(&args) {
        std::process::exit(if sparkdev::mcp::serve(&client).is_ok() {
            0
        } else {
            3
        });
    }
    let json_mode = args.iter().any(|a| a == "--json");
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
