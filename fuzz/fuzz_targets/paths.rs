#![no_main]

use libfuzzer_sys::fuzz_target;
use scion_types::RepoPath;

fuzz_target!(|input: &str| {
    let parsed = RepoPath::new(input);
    let json = serde_json::to_string(input).unwrap();
    let wire = serde_json::from_str::<RepoPath>(&json);
    assert_eq!(parsed.is_ok(), wire.is_ok());
    if let Ok(path) = parsed {
        assert_eq!(wire.unwrap(), path);
        assert!(RepoPath::root().contains(&path));
        assert!(path.contains(&path));
        assert!(!path.as_str().contains(['\\', '\0', ':']));
        for component in path.components() {
            assert!(!component.is_empty());
            assert_ne!(component, ".");
            assert_ne!(component, "..");
            assert!(!component.eq_ignore_ascii_case(".git"));
            assert!(!component.eq_ignore_ascii_case(".scion"));
        }
    }
});
