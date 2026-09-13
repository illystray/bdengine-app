use discord_rich_presence::{activity::Activity, DiscordIpc, DiscordIpcClient};
use std::{io, sync::mpsc, thread};

enum Request {
  Set(Activity<'static>),
  Clear,
  Shutdown,
}

/// Only the worker owns the IPC client. The UI never waits for Discord or its mutex.
pub struct DiscordPresence {
  sender: mpsc::Sender<Request>,
}

impl DiscordPresence {
  pub fn new(application_id: &'static str) -> io::Result<Self> {
    let mut client: Option<DiscordIpcClient> = None;
    Self::start(move |request| match request {
      Request::Set(activity) => {
        for _ in 0..2 {
          if client.is_none() {
            let mut connection = DiscordIpcClient::new(application_id);
            if connection.connect().is_err() {
              return;
            }
            client = Some(connection);
          }

          if client
            .as_mut()
            .unwrap()
            .set_activity(activity.clone())
            .is_ok()
          {
            return;
          }
          client = None;
        }
      }
      Request::Clear => {
        if let Some(mut connection) = client.take() {
          let _ = connection.clear_activity();
          // Dropping the pipe disconnects without the blocking flush in close().
        }
      }
      Request::Shutdown => unreachable!("shutdown is handled by the worker loop"),
    })
  }

  fn start(mut handle: impl FnMut(Request) + Send + 'static) -> io::Result<Self> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
      .name("discord-presence".into())
      .spawn(move || {
        while let Ok(mut request) = receiver.recv() {
          // If Discord was slow, send the latest desired status instead of a backlog.
          while !matches!(request, Request::Shutdown) {
            match receiver.try_recv() {
              Ok(next) => request = next,
              Err(_) => break,
            }
          }
          if matches!(request, Request::Shutdown) {
            break;
          }
          handle(request);
        }
        // The client is dropped on this thread. Never join it from the UI on exit.
      })?;
    Ok(Self { sender })
  }

  pub fn set(&self, activity: Activity<'static>) -> Result<(), String> {
    self.send(Request::Set(activity))
  }

  pub fn clear(&self) -> Result<(), String> {
    self.send(Request::Clear)
  }

  pub fn shutdown(&self) {
    let _ = self.sender.send(Request::Shutdown);
  }

  fn send(&self, request: Request) -> Result<(), String> {
    self
      .sender
      .send(request)
      .map_err(|_| "Discord worker is unavailable.".into())
  }
}

impl Drop for DiscordPresence {
  fn drop(&mut self) {
    self.shutdown();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{Duration, Instant};

  #[test]
  fn stalled_discord_does_not_block_updates_clear_or_shutdown() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let presence = DiscordPresence::start(move |_| {
      let _ = entered_tx.send(());
      let _ = release_rx.recv_timeout(Duration::from_secs(3));
    })
    .unwrap();
    presence.set(Activity::new()).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    // Simulate a handshake still waiting on Discord while the user changes mode or exits.
    let started = Instant::now();
    presence.set(Activity::new()).unwrap();
    presence.clear().unwrap();
    presence.shutdown();
    drop(presence);
    let elapsed = started.elapsed();
    let _ = release_tx.send(());
    assert!(
      elapsed < Duration::from_millis(250),
      "UI dispatch waited {elapsed:?} for Discord"
    );
  }

  #[test]
  fn clear_supersedes_status_updates_queued_during_a_slow_handshake() {
    let (handled_tx, handled_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut first = true;
    let presence = DiscordPresence::start(move |request| {
      let _ = handled_tx.send(matches!(request, Request::Clear));
      if first {
        first = false;
        let _ = release_rx.recv_timeout(Duration::from_secs(3));
      }
    })
    .unwrap();
    presence.set(Activity::new()).unwrap();
    assert!(!handled_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    presence.set(Activity::new()).unwrap();
    presence.set(Activity::new()).unwrap();
    presence.clear().unwrap();
    release_tx.send(()).unwrap();
    assert!(handled_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    assert!(handled_rx.try_recv().is_err());
  }
}
