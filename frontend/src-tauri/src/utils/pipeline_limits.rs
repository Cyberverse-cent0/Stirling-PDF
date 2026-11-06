use std::fmt;

/// Pipeline caps; a non-positive value leaves its corresponding cap unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineLimits {
    /// Maximum number of steps in one pipeline.
    pub max_steps: i32,
    /// Maximum number of files produced by one step.
    pub max_files_per_step: i32,
}

impl PipelineLimits {
    /// Creates limits from the configured step and per-step file caps.
    pub const fn new(max_steps: i32, max_files_per_step: i32) -> Self {
        Self {
            max_steps,
            max_files_per_step,
        }
    }

    pub const fn exceeds_step_limit(self, steps: i32) -> bool {
        self.max_steps > 0 && steps > self.max_steps
    }

    pub fn step_limit_message(self) -> String {
        format!(
            "A pipeline may have at most {} steps; raise policies.maxSteps to allow more",
            self.max_steps
        )
    }

    pub fn require_within_step_limit(self, steps: i32) -> Result<(), LimitExceededError> {
        if self.exceeds_step_limit(steps) {
            return Err(LimitExceededError(self.step_limit_message()));
        }
        Ok(())
    }

    pub fn require_within_file_limit(
        self,
        operation: &str,
        files: i32,
    ) -> Result<(), LimitExceededError> {
        if self.max_files_per_step > 0 && files > self.max_files_per_step {
            return Err(LimitExceededError(format!(
                "Step {operation} produced more than {} files; raise policies.maxFilesPerStep to allow more",
                self.max_files_per_step
            )));
        }
        Ok(())
    }
}

/// A pipeline run exceeded one of its configured caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitExceededError(String);

impl LimitExceededError {
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LimitExceededError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for LimitExceededError {}

#[cfg(test)]
mod tests {
    use super::{LimitExceededError, PipelineLimits};

    #[test]
    fn step_limit_allows_the_limit_and_rejects_excess() {
        let limits = PipelineLimits::new(2, 10);

        assert!(!limits.exceeds_step_limit(2));
        assert!(limits.exceeds_step_limit(3));
        assert!(limits.require_within_step_limit(2).is_ok());
        assert_eq!(
            limits.require_within_step_limit(3).unwrap_err().message(),
            "A pipeline may have at most 2 steps; raise policies.maxSteps to allow more"
        );
    }

    #[test]
    fn non_positive_limits_are_unlimited() {
        for limit in [0, -1] {
            let limits = PipelineLimits::new(limit, limit);
            assert!(!limits.exceeds_step_limit(1_000));
            assert!(limits.require_within_step_limit(1_000).is_ok());
            assert!(limits.require_within_file_limit("merge", 1_000).is_ok());
        }
    }

    #[test]
    fn file_limit_error_names_operation_and_setting() {
        let limits = PipelineLimits::new(10, 4);

        assert!(limits.require_within_file_limit("split", 4).is_ok());
        let error: LimitExceededError = limits.require_within_file_limit("split", 5).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Step split produced more than 4 files; raise policies.maxFilesPerStep to allow more"
        );
    }
}
