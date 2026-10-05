//! A read oplock is an advisory change notification, not a write lock. In
//! particular it catches writes within one filesystem timestamp tick.
use std::{
  cell::UnsafeCell,
  fs::File,
  mem::size_of,
  os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows::Win32::{
  Foundation::{ERROR_IO_PENDING, HANDLE, WAIT_TIMEOUT},
  System::{
    Ioctl::{
      FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_READ, REQUEST_OPLOCK_CURRENT_VERSION,
      REQUEST_OPLOCK_INPUT_BUFFER, REQUEST_OPLOCK_INPUT_FLAG_REQUEST, REQUEST_OPLOCK_OUTPUT_BUFFER,
    },
    Threading::{CreateEventW, WaitForSingleObject},
    IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED},
  },
};

pub(super) struct FileWatch {
  file: File,
  event: OwnedHandle,
  overlapped: Box<UnsafeCell<OVERLAPPED>>,
  _output: Box<UnsafeCell<REQUEST_OPLOCK_OUTPUT_BUFFER>>,
}

// The OS owns the pending I/O buffers until Drop cancels and joins that I/O.
// We never read those buffers concurrently; changed() only waits on the event.
unsafe impl Send for FileWatch {}
unsafe impl Sync for FileWatch {}

impl FileWatch {
  pub fn new(file: &File) -> Option<Self> {
    unsafe {
      let file = file.try_clone().ok()?;
      let raw_event = CreateEventW(None, true, false, None).ok()?;
      let event = OwnedHandle::from_raw_handle(raw_event.0);
      let overlapped = Box::new(UnsafeCell::new(OVERLAPPED {
        hEvent: raw_event,
        ..Default::default()
      }));
      let output = Box::new(UnsafeCell::new(REQUEST_OPLOCK_OUTPUT_BUFFER::default()));
      let input = REQUEST_OPLOCK_INPUT_BUFFER {
        StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
        StructureLength: size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u16,
        RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ,
        Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
      };
      let result = DeviceIoControl(
        HANDLE(file.as_raw_handle()),
        FSCTL_REQUEST_OPLOCK,
        Some((&input as *const REQUEST_OPLOCK_INPUT_BUFFER).cast()),
        size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u32,
        Some(output.get().cast()),
        size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u32,
        None,
        Some(overlapped.get()),
      );
      if result.err().map(|err| err.code()) != Some(ERROR_IO_PENDING.to_hresult()) {
        return None;
      }
      Some(Self {
        file,
        event,
        overlapped,
        _output: output,
      })
    }
  }

  pub fn changed(&self) -> bool {
    unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), 0) != WAIT_TIMEOUT }
  }
}

impl Drop for FileWatch {
  fn drop(&mut self) {
    unsafe {
      let handle = HANDLE(self.file.as_raw_handle());
      let _ = CancelIoEx(handle, Some(self.overlapped.get()));
      let mut transferred = 0;
      let _ = GetOverlappedResult(handle, self.overlapped.get(), &mut transferred, true);
    }
  }
}
