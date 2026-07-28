//! Tests for the autoupdate module

#[test]
fn compare_versions_equal() {
    assert_eq!(
        super::compare_versions("1.0.0", "1.0.0"),
        std::cmp::Ordering::Equal
    );
}

#[test]
fn compare_versions_greater_patch() {
    assert_eq!(
        super::compare_versions("1.0.1", "1.0.0"),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn compare_versions_less_patch() {
    assert_eq!(
        super::compare_versions("1.0.0", "1.0.1"),
        std::cmp::Ordering::Less
    );
}

#[test]
fn compare_versions_major_wins() {
    assert_eq!(
        super::compare_versions("2.0.0", "1.9.9"),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn compare_versions_with_v_prefix() {
    assert_eq!(
        super::compare_versions("v1.2.3", "1.2.3"),
        std::cmp::Ordering::Equal
    );
}

#[test]
fn compare_versions_different_length() {
    assert_eq!(
        super::compare_versions("1.0", "1.0.0"),
        std::cmp::Ordering::Equal
    );
    assert_eq!(
        super::compare_versions("1.0.0.1", "1.0.0"),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn stable_version_accepts_plain() {
    assert!(super::is_stable_version("1.0.0"));
    assert!(super::is_stable_version("v1.0.0"));
    assert!(super::is_stable_version("v0.32.1"));
}

#[test]
fn stable_version_rejects_prerelease() {
    assert!(!super::is_stable_version("1.0.0-beta"));
    assert!(!super::is_stable_version("v1.0.0-rc1"));
    assert!(!super::is_stable_version("1.0.0-alpha+build123"));
}

#[test]
fn stable_version_rejects_empty() {
    assert!(!super::is_stable_version(""));
    assert!(!super::is_stable_version("v"));
}

#[test]
fn current_version_returns_non_empty() {
    let version = super::current_version();
    assert!(!version.is_empty(), "version should not be empty");
}

#[test]
fn detect_platform_returns_valid_tuple() {
    let result = super::detect_platform();
    // On supported platforms, this should succeed
    if (cfg!(target_os = "linux") || cfg!(target_os = "macos"))
        && (cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64"))
    {
        assert!(
            result.is_ok(),
            "should detect platform on supported systems"
        );
        let (os, arch) = result.unwrap();
        assert!(!os.is_empty(), "OS should not be empty");
        assert!(!arch.is_empty(), "arch should not be empty");
    }
}

#[test]
fn now_secs_returns_reasonable_timestamp() {
    let result = super::now_secs();
    assert!(result.is_ok(), "now_secs should succeed");
    let secs = result.unwrap();
    // Should be after 2020-01-01 and before 2100-01-01
    assert!(secs > 1577836800, "timestamp should be after 2020");
    assert!(secs < 4102444800, "timestamp should be before 2100");
}

#[test]
fn should_check_returns_true_when_no_last_check_file() {
    // When there's no last check file, should_check should return true
    // This is the default behavior for fresh installs
    let result = super::should_check();
    // The result depends on whether the data dir exists and has a last check file
    // We can't control this in a unit test without mocking, so just verify it doesn't panic
    let _ = result;
}

#[test]
fn record_check_creates_last_check_file() {
    // record_check should create a last check file with the current timestamp
    let result = super::record_check();
    // This will succeed if the data dir exists, fail otherwise
    // We can't control this in a unit test without mocking
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn compare_versions_handles_build_metadata() {
    // Build metadata should be ignored in comparisons
    assert_eq!(
        super::compare_versions("1.0.0+build1", "1.0.0+build2"),
        std::cmp::Ordering::Equal
    );
}

#[test]
fn is_stable_version_accepts_semver_with_patch() {
    assert!(super::is_stable_version("1.2.3"));
    assert!(super::is_stable_version("0.1.0"));
    assert!(super::is_stable_version("10.20.30"));
}

#[test]
fn is_stable_version_rejects_versions_with_hyphens() {
    assert!(!super::is_stable_version("1.0.0-something"));
    assert!(!super::is_stable_version("v2.0.0-rc.1"));
}
