//! Explicit selected-computer provisioning through its authenticated endpoint.
use crate::{AppError, Result, bundle, inventory, transport::Transport};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn invalid() -> AppError {
    AppError::new(
        3,
        "The validated setup bundle is missing its bootstrap program",
    )
}

/// Extra computers use pinned SSH. The packaged link executable targets ARM64,
/// so never install it or a paired-link unit on another selected computer.
/// The legacy Spark desktop bootstrap retains its existing pairing behavior.
fn selected_bundle(mut bundle: Value, cuda: bool) -> Result<(Value, Vec<u8>)> {
    let file = &bundle["files"]["bootstrap.py"];
    let script = STANDARD
        .decode(file["data"].as_str().ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    if script.is_empty()
        || script.len() as u64 > inventory::MIB
        || file["sha256"].as_str() != Some(inventory::hash(&script).as_str())
    {
        return Err(invalid());
    }
    let files = bundle["files"].as_object_mut().ok_or_else(invalid)?;
    files.remove("bin/sparkdev-link");
    files.remove("sparkdev-link.service");
    if cuda
        && !(files.contains_key("install-cuda.py") && files.contains_key("runtime/Dockerfile.cuda"))
    {
        return Err(AppError::new(
            3,
            "The validated setup bundle has no GPU preparation files",
        ));
    }
    if cuda {
        bundle["build_cuda"] = json!(true);
    } else if let Some(object) = bundle.as_object_mut() {
        object.remove("build_cuda");
    }
    bundle["relay_mode"] = json!("disabled");
    Ok((bundle, script))
}

pub(crate) fn bootstrap(resources: &Path, cuda: bool) -> Result<Value> {
    // Loading validates the entire closed file set before any remote command.
    let bundle = bundle::load(resources).map_err(|message| AppError::new(3, message))?;
    let (bundle, script) = selected_bundle(bundle, cuda)?;
    let mut transport = Transport::new()?;
    let command = transport.bootstrap_command(&script)?;
    let response = transport.ssh_raw(
        &command,
        &serde_json::to_vec(&bundle)?,
        Duration::from_secs(90),
    )?;
    let mut result = Transport::decode(&response)?;
    result["transport"] = transport.info();
    result["paired_link_payload_omitted"] = json!(true);
    result["relay_mode"] = json!("disabled");
    result["gpu_preparation_requested"] = json!(cuda);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        let script = b"print('validated bootstrap fixture')\n";
        json!({"v":1,"build_runtime":true,"files":{
            "bootstrap.py":{"data":STANDARD.encode(script),"sha256":inventory::hash(script)},
            "controller.py":{"data":"aW5zcGVjdGFibGU=","sha256":"retained"},
            "install-cuda.py":{"data":"Y3VkYQ==","sha256":"retained-cuda-installer"},
            "runtime/Dockerfile.cuda":{"data":"Y3VkYQ==","sha256":"retained-cuda-recipe"},
            "bin/sparkdev-link":{"data":"YXJtNjQ=","sha256":"arm64-only"},
            "sparkdev-link.service":{"data":"dW5pdA==","sha256":"paired-link"}
        }})
    }

    #[test]
    fn extra_node_bundle_keeps_controller_and_omits_arm_pairing_payload() {
        let original = fixture();
        let controller = original["files"]["controller.py"].clone();
        let (bundle, script) = selected_bundle(original, false).unwrap();
        assert_eq!(script, b"print('validated bootstrap fixture')\n");
        assert_eq!(bundle["relay_mode"], "disabled");
        assert_eq!(bundle["build_runtime"], true);
        assert_eq!(bundle["files"]["controller.py"], controller);
        assert!(bundle["files"].get("bin/sparkdev-link").is_none());
        assert!(bundle["files"].get("sparkdev-link.service").is_none());
        assert!(bundle.get("build_cuda").is_none());
        let (again, _) = selected_bundle(bundle.clone(), false).unwrap();
        assert_eq!(again, bundle);
    }

    #[test]
    fn bootstrap_script_must_be_bounded_decodable_and_hash_verified() {
        for changed in [json!("not base64"), json!(STANDARD.encode([])), json!(42)] {
            let mut bundle = fixture();
            bundle["files"]["bootstrap.py"]["data"] = changed;
            assert!(selected_bundle(bundle, false).is_err());
        }
        let mut bundle = fixture();
        bundle["files"]["bootstrap.py"]["sha256"] = json!("different");
        assert!(selected_bundle(bundle, false).is_err());
        assert!(selected_bundle(json!({"v":1,"files":{}}), false).is_err());
    }

    #[test]
    fn first_gpu_preparation_requires_an_explicit_flag_and_setup_files() {
        let (enabled, _) = selected_bundle(fixture(), true).unwrap();
        assert_eq!(enabled["build_cuda"], true);
        let (disabled, _) = selected_bundle(enabled, false).unwrap();
        assert!(disabled.get("build_cuda").is_none());
        let mut missing = fixture();
        missing["files"]
            .as_object_mut()
            .unwrap()
            .remove("install-cuda.py");
        assert!(selected_bundle(missing, true).is_err());
    }
}
