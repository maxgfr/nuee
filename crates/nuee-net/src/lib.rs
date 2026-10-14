//! iroh network layer of Nuée: endpoint setup, stream framing, admission and the driver
//! that connects the `nuee-core` state machines to real connections.
#![forbid(unsafe_code)]

pub mod admit;
pub mod console;
pub mod endpoint;
pub mod framing;
pub mod rtc;

pub use admit::{Admission, AdmitConfig, AdmitError};
pub use console::{
    MAX_CONSOLE_HELLO, console_exchange, recv_console_body, recv_console_hello,
    send_console_request,
};
pub use endpoint::{Net, NetConfig, NetError, ONLINE_TIMEOUT, RelayChoice};
pub use framing::{FrameError, read_frame, recv_msg, send_msg, write_frame};
pub use iroh::{EndpointAddr, RelayUrl};
pub mod driver;

pub use driver::{
    BlobError, BlobHub, CoordinatorDriver, CoordinatorHandle, DriverConfig, HostEvent, JoinError,
    LaneInError, MemBlobHub, TaskExec, WorkerHandle, WorkerHostEvent, join,
};
