use std::fs;
use std::path::Path;

#[test]
fn java_stress_fixtures_do_not_use_unbounded_barriers() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/java/data");
    let entries = fs::read_dir(&fixture_dir).expect("read java fixture directory");
    let mut offenders = Vec::new();

    for entry in entries {
        let entry = entry.expect("read java fixture entry");
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("java") {
            continue;
        }
        let body = fs::read_to_string(&path).expect("read java fixture");
        for (line_idx, line) in body.lines().enumerate() {
            if line.contains("barrier.await();") {
                offenders.push(format!(
                    "{}:{}",
                    path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .unwrap_or(&path)
                        .display(),
                    line_idx + 1
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "Java stress fixtures must use timed CyclicBarrier waits so worker failures cannot hang BDD forever: {}",
        offenders.join(", ")
    );
}
