use loom::sync::{Arc, Mutex};
use loom::thread;

#[derive(Debug, Default)]
struct MutationRecord {
    received: bool,
    durable_commits: u8,
}

#[test]
fn received_crash_and_retry_never_duplicate_a_durable_mutation() {
    loom::model(|| {
        let record = Arc::new(Mutex::new(MutationRecord::default()));
        let submit = Arc::clone(&record);
        let retry = Arc::clone(&record);
        let first = thread::spawn(move || {
            let mut record = submit.lock().expect("submit lock");
            record.received = true;
            if record.durable_commits == 0 {
                record.durable_commits = 1;
            }
        });
        let second = thread::spawn(move || {
            let mut record = retry.lock().expect("retry lock");
            if record.durable_commits == 0 {
                record.durable_commits = 1;
            }
        });
        first.join().expect("first");
        second.join().expect("second");
        let record = record.lock().expect("final lock");
        assert!(record.received);
        assert_eq!(record.durable_commits, 1);
    });
}
