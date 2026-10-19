use std::{fs, process::Command};
use tempfile::tempdir;

#[test]
fn offline_org_validator_checks_prices_purposes_and_model_references() {
    let root = tempdir().unwrap();
    let models = root.path().join("models");
    let orgs = root.path().join("orgs");
    fs::create_dir(&models).unwrap();
    fs::create_dir(&orgs).unwrap();
    fs::write(models.join("model.yaml"),"model: org/model\nclay:\n  alias: org/model\n  deployments:\n    - {alias: provider, model_name: org/model, endpoint: onwards}\n  routing:\n    pools:\n      default:\n        - deployment: provider\n").unwrap();
    let base = "org: example\nmodels:\n  - alias: org/model\n    class_pricing:\n      interactive:\n        tariffs:\n          - {name: price, purpose: realtime, input_per_million_tokens: '1', output_per_million_tokens: '2'}\n        cache_tariff: {write_multiplier_5m: '1', write_multiplier_1h: '1', write_multiplier_24h: '1', read_multiplier: '0.10000'}\n";
    for (yaml, valid, diagnostic) in [
        (base.to_owned(), true, ""),
        (
            base.replace("alias: org/model", "alias: missing"),
            false,
            "absent from the model catalog",
        ),
        (base.replace("0.10000", "0.12345"), false, "DECIMAL(6,4)"),
        (base.replace("purpose: realtime", "purpose: platform"), false, "platform"),
        (
            base.replace("purpose: realtime", "purpose: batch, completion_window: 24h"),
            false,
            "standard",
        ),
        (base.replace("interactive:", "unknown-class:"), false, "unknown-class"),
        (
            base.replace("interactive:", "standard:")
                .replace("purpose: realtime", "purpose: batch, completion_window: ' 24h '"),
            false,
            "completion_window",
        ),
    ] {
        fs::write(orgs.join("customer.yaml"), yaml).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_dwctl-model-provisioning"))
            .arg("validate-org-overlays")
            .arg(&orgs)
            .arg("--models")
            .arg(&models)
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), valid, "{error}");
        if !valid {
            assert!(error.contains(diagnostic), "{error}");
        }
    }
}
