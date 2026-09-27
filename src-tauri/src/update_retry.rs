use std::{error::Error, time::Duration};

const RETRY_DELAYS: [Duration; 3] = [
  Duration::from_secs(2),
  Duration::from_secs(5),
  Duration::from_secs(10),
];

pub(crate) struct Failure {
  message: String,
  retryable: bool,
}

impl Failure {
  pub(crate) fn new(message: impl Into<String>, retryable: bool) -> Self {
    Self {
      message: message.into(),
      retryable,
    }
  }

  pub(crate) fn from_error(context: &str, error: &dyn Error, retryable: bool) -> Self {
    let mut message = format!("{context}: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
      let detail = cause.to_string();
      if !message.ends_with(&detail) {
        message.push_str(": ");
        message.push_str(&detail);
      }
      source = cause.source();
    }
    Self::new(message, retryable)
  }
}

pub(crate) fn run<T>(
  mut attempt: impl FnMut() -> Result<T, Failure>,
  mut wait: impl FnMut(Duration),
) -> Result<T, String> {
  for index in 0..=RETRY_DELAYS.len() {
    match attempt() {
      Ok(value) => return Ok(value),
      Err(failure) => {
        if !failure.retryable || index == RETRY_DELAYS.len() {
          return Err(format!(
            "Update download failed after {} attempt(s): {}",
            index + 1,
            failure.message
          ));
        }
        wait(RETRY_DELAYS[index]);
      }
    }
  }
  unreachable!("The final failed attempt returns above")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn successful_first_attempt_does_not_wait() {
    assert_eq!(run(|| Ok(7), |_| panic!("Unexpected retry")), Ok(7));
  }

  #[test]
  fn recovers_on_fourth_attempt_with_three_delays() {
    let mut attempts = 0;
    let mut delays = Vec::new();
    let result = run(
      || {
        attempts += 1;
        if attempts < 4 {
          Err(Failure::new("Network stalled", true))
        } else {
          Ok(())
        }
      },
      |delay| delays.push(delay),
    );
    assert!(result.is_ok());
    assert_eq!(attempts, 4);
    assert_eq!(delays, RETRY_DELAYS);
  }

  #[test]
  fn persistent_failure_stops_after_four_attempts() {
    let mut attempts = 0;
    let mut waits = 0;
    let result: Result<(), _> = run(
      || {
        attempts += 1;
        Err(Failure::new("Network stalled", true))
      },
      |_| waits += 1,
    );
    assert_eq!(attempts, 4);
    assert_eq!(waits, 3);
    assert!(result
      .unwrap_err()
      .contains("after 4 attempt(s): Network stalled"));
  }

  #[test]
  fn terminal_failure_does_not_retry() {
    let result: Result<(), _> = run(
      || Err(Failure::new("Disk full", false)),
      |_| panic!("Local file errors must not retry"),
    );
    assert!(result
      .unwrap_err()
      .contains("after 1 attempt(s): Disk full"));
  }

  #[test]
  fn preserves_nested_error_cause() {
    #[derive(Debug)]
    struct ReadError(std::io::Error);
    impl std::fmt::Display for ReadError {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("error decoding response body")
      }
    }
    impl Error for ReadError {
      fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
      }
    }
    let failure = Failure::from_error(
      "Could not read update stream",
      &ReadError(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "operation timed out",
      )),
      true,
    );
    assert_eq!(
      failure.message,
      "Could not read update stream: error decoding response body: operation timed out"
    );
  }
}
