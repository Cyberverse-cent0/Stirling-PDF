use std::cell::RefCell;

thread_local! {
    static CURRENT_JOB_ID: RefCell<Option<String>> = const { RefCell::new(None) };
    static CURRENT_OWNER: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Stores a job ID for the current thread.
pub fn set_job_id(job_id: Option<&str>) {
    CURRENT_JOB_ID.with(|current| {
        *current.borrow_mut() = job_id.map(str::to_owned);
    });
}

/// Returns the job ID stored for the current thread.
pub fn get_job_id() -> Option<String> {
    CURRENT_JOB_ID.with(|current| current.borrow().clone())
}

/// Stores an owner for the current thread.
pub fn set_owner(owner: Option<&str>) {
    CURRENT_OWNER.with(|current| {
        *current.borrow_mut() = owner.map(str::to_owned);
    });
}

/// Returns the owner stored for the current thread.
pub fn get_owner() -> Option<String> {
    CURRENT_OWNER.with(|current| current.borrow().clone())
}

/// Clears both values from the current thread.
pub fn clear() {
    CURRENT_JOB_ID.with(|current| *current.borrow_mut() = None);
    CURRENT_OWNER.with(|current| *current.borrow_mut() = None);
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::{clear, get_job_id, get_owner, set_job_id, set_owner};

    #[test]
    fn stores_and_overwrites_context_values() {
        clear();
        assert_eq!(get_job_id(), None);
        assert_eq!(get_owner(), None);

        set_job_id(Some("job-123"));
        set_owner(Some("alice"));
        assert_eq!(get_job_id().as_deref(), Some("job-123"));
        assert_eq!(get_owner().as_deref(), Some("alice"));

        set_job_id(Some("job-456"));
        assert_eq!(get_job_id().as_deref(), Some("job-456"));
        clear();
        assert_eq!(get_job_id(), None);
        assert_eq!(get_owner(), None);
    }

    #[test]
    fn isolates_values_between_threads() {
        clear();
        set_job_id(Some("main-job"));
        set_owner(Some("main-owner"));

        let other_thread = thread::spawn(|| {
            assert_eq!(get_job_id(), None);
            assert_eq!(get_owner(), None);
            set_job_id(Some("other-job"));
            set_owner(Some("other-owner"));
            assert_eq!(get_job_id().as_deref(), Some("other-job"));
            assert_eq!(get_owner().as_deref(), Some("other-owner"));
        });
        other_thread.join().unwrap();

        assert_eq!(get_job_id().as_deref(), Some("main-job"));
        assert_eq!(get_owner().as_deref(), Some("main-owner"));
        clear();
    }

    #[test]
    fn setting_none_clears_one_value() {
        set_job_id(Some("job"));
        set_owner(Some("owner"));

        set_job_id(None);
        assert_eq!(get_job_id(), None);
        assert_eq!(get_owner().as_deref(), Some("owner"));
        clear();
    }
}
