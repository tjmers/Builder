use crate::ip_address::IpAddress;

#[cfg(windows)]
pub type NativeSocket = usize;

#[cfg(target_os = "linux")]
pub type NativeSocket = libc::c_int;

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("OS Not supported");

// Platform implementations of new, bind, valid, run, and drop
#[cfg(target_os = "linux")]
mod linux;

pub type RequestHandler<'a> = Box<dyn Fn(&str, IpAddress) -> String + 'a>;

pub struct TcpListener<'a> {
    socket: NativeSocket,
    handler: RequestHandler<'a>,
    port: i32,
}
