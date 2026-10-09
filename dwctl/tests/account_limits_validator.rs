use std::{fs, process::Command};
use tempfile::tempdir;

#[test]
fn offline_account_limits_validator_checks_limits_accounts_and_model_references() {
    let root = tempdir().unwrap();
    let models = root.path().join("models");
    fs::create_dir(&models).unwrap();
    fs::write(models.join("model.yaml"),"model: org/model\nclay:\n  alias: org/model\n  deployments:\n    - {alias: provider, model_name: org/model, endpoint: onwards}\n  routing:\n    pools:\n      default:\n        - deployment: provider\n").unwrap();
    let base = "account: example\nrealtime_inflight:\n  org/model: 70\n";
    for (files, valid, diagnostic) in [
        (vec![base.to_owned()], true, ""),
        (vec![base.replace("org/model", "missing")], false, "absent from the model catalog"),
        (vec![base.replace("70", "0")], false, "must be positive"),
        (vec![base.replace("realtime_inflight", "realtime")], false, "unknown field"),
        (vec![base.to_owned(), base.to_owned()], false, "also declared"),
        // A pinned toleration that is valid on its own is accepted.
        (
            vec![format!(
                "{base}pinned_tolerations:\n  - {{key: dedicated, value: only, effect: NoSchedule}}\n"
            )],
            true,
            "",
        ),
        // An `Exists` toleration must not carry a value.
        (
            vec![format!(
                "{base}pinned_tolerations:\n  - {{key: dedicated, operator: Exists, value: nope}}\n"
            )],
            false,
            "must not carry a value",
        ),
    ] {
        let accounts = tempdir().unwrap();
        for (index, contents) in files.iter().enumerate() {
            fs::write(accounts.path().join(format!("account-{index}.yaml")), contents).unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_dwctl-model-provisioning"))
            .arg("validate-account-limits")
            .arg(accounts.path())
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
