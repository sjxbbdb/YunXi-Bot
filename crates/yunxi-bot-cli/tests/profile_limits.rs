use std::process::Command;

#[test]
fn a_confirmed_profile_can_grow_beyond_8k_without_losing_existing_text() {
    let home = std::env::temp_dir().join(format!("yunxi-profile-grow-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let original = "合成偏好。".repeat(700);
    assert!(original.len() > 8 * 1024 && original.len() < 32 * 1024);
    let path = home.join("profile.md");
    std::fs::write(&path, &original).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_yunxi-bot"))
            .env("YUNXI_BOT_HOME", &home)
            .args(args)
            .output()
            .unwrap()
    };
    let text = "合成偏好：喜欢清淡口味";
    assert!(run(&["profile", "--propose", text]).status.success());
    let pending = run(&["profile", "--pending"]);
    let pending_text = String::from_utf8(pending.stdout).unwrap();
    let id = pending_text
        .split('[')
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let accepted = run(&["profile", "--accept", id]);
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{original}\n- {text}\n")
    );
    let loaded = yunxi_bot_core::profile::load(&home).unwrap();
    assert!(!loaded.truncated);
    assert!(loaded.text.ends_with(&format!("- {text}\n")));
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn accepting_an_oversized_profile_keeps_the_file_and_proposal_unchanged() {
    let home = std::env::temp_dir().join(format!("yunxi-profile-limit-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let original = "x".repeat(yunxi_bot_core::profile::MAX_PROFILE_BYTES - 1);
    let path = home.join("profile.md");
    std::fs::write(&path, &original).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_yunxi-bot"))
            .env("YUNXI_BOT_HOME", &home)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(
        run(&["profile", "--propose", "合成偏好：喜欢清淡口味"])
            .status
            .success()
    );
    let pending = run(&["profile", "--pending"]);
    assert!(pending.status.success());
    let pending_text = String::from_utf8(pending.stdout).unwrap();
    let id = pending_text
        .split('[')
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let accepted = run(&["profile", "--accept", id]);
    assert_eq!(accepted.status.code(), Some(2));
    assert!(String::from_utf8(accepted.stderr).unwrap().contains("上限"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    let still_pending = run(&["profile", "--pending"]);
    assert_eq!(
        String::from_utf8(still_pending.stdout).unwrap(),
        pending_text
    );
    std::fs::remove_dir_all(home).unwrap();
}
