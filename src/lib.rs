//! Distributed programming primitives.
//!
//! This library provides a runtime to aide in the writing and debugging of distributed programs.
//!
//! The two key ideas are:
//!
//!  * **Spawning new processes:** The [`spawn()`](spawn) function can be used to spawn a new process running a particular function.
//!  * **Channels:** [Sender]s and [Receiver]s can be used for synchronous or asynchronous inter-process communication.
//!
//! The only requirement to use is that [`init()`](init) must be called immediately inside your application's `main()` function.

#![doc(html_root_url = "https://docs.rs/constellation-rs/0.1.4")]
#![feature(
	read_initializer,
	core_intrinsics,
	nll,
	arbitrary_self_types,
	futures_api,
	pin,
	unboxed_closures,
	fnbox,
	try_from,
	never_type
)]
#![warn(
	missing_copy_implementations,
	// missing_debug_implementations,
	missing_docs,
	trivial_numeric_casts,
	unused_extern_crates,
	unused_import_braces,
	unused_qualifications,
	unused_results,
	clippy::pedantic
)] // from https://github.com/rust-unofficial/patterns/blob/master/anti_patterns/deny-warnings.md
#![allow(
	dead_code,
	clippy::match_ref_pats,
	clippy::inline_always,
	clippy::or_fun_call,
	clippy::similar_names,
	clippy::if_not_else,
	clippy::stutter,
	clippy::new_ret_no_self,
	clippy::type_complexity,
	clippy::cast_ptr_alignment,
	clippy::explicit_write
)]

extern crate atty;
extern crate bincode;
extern crate constellation_internal;
extern crate either;
// extern crate futures;
extern crate get_env;
extern crate nix;
extern crate notifier;
extern crate palaver;
extern crate proc_self;
extern crate rand;
extern crate serde;
extern crate serde_json;
extern crate serde_pipe;
extern crate tcp_typed;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_closure;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;

mod channel;

use constellation_internal::{
	map_bincode_err, BufferedStream, Deploy, DeployOutputEvent, Envs, ExitStatus, Format, Formatter, PidInternal, ProcessInputEvent, ProcessOutputEvent, StyleSupport
};
use either::Either;
use nix::{
	errno, fcntl, libc, sys::{
		signal, socket::{self, sockopt}, stat, wait
	}, unistd
};
use palaver::{
	copy_sendfile, fexecve, is_valgrind, memfd_create, socket, spawn as thread_spawn, valgrind_start_fd, SockFlag
};
use proc_self::{exe, exe_path, fd_path, FdIter};
use std::{
	alloc, borrow, cell, convert::TryInto, ffi::{CString, OsString}, fmt, fs, intrinsics, io::{self, Read, Write}, iter, marker, mem, net, ops, os::{
		self, unix::{
			ffi::OsStringExt, io::{AsRawFd, FromRawFd, IntoRawFd}
		}
	}, path, process, str, sync::{self, mpsc}, thread
};

#[cfg(target_family = "unix")]
type Fd = os::unix::io::RawFd;
#[cfg(target_family = "windows")]
type Fd = os::windows::io::RawHandle;

pub use channel::{ChannelError, Selectable};
pub use constellation_internal::{Pid, Resources, RESOURCES_DEFAULT};

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

const LISTENER_FD: Fd = 3; // from fabric
const ARG_FD: Fd = 4; // from fabric
const SCHEDULER_FD: Fd = 4;
const MONITOR_FD: Fd = 5;

#[derive(Clone, Deserialize, Debug)]
struct SchedulerArg {
	scheduler: net::SocketAddr,
}

lazy_static! {
	static ref BRIDGE: sync::RwLock<Option<Pid>> = sync::RwLock::new(None);
	static ref SCHEDULER: sync::Mutex<()> = sync::Mutex::new(());
	static ref DEPLOYED: sync::RwLock<Option<bool>> = sync::RwLock::new(None);
	static ref REACTOR: sync::RwLock<Option<channel::Reactor>> = sync::RwLock::new(None);
	static ref RESOURCES: sync::RwLock<Option<Resources>> = sync::RwLock::new(None);
	static ref HANDLE: sync::RwLock<Option<channel::Handle>> = sync::RwLock::new(None);
}

#[global_allocator]
static GLOBAL_ALLOCATOR: alloc::System = alloc::System;

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

/// The sending half of a channel.
///
/// It has a synchronous blocking method [`send()`](Sender::send) and an asynchronous nonblocking method [`selectable_send()`](Sender::selectable_send).
pub struct Sender<T: serde::ser::Serialize>(Option<channel::Sender<T>>, Pid);
impl<T: serde::ser::Serialize> Sender<T> {
	/// Create a new `Sender<T>` with a remote [Pid]. This method returns instantly.
	pub fn new(remote: Pid) -> Self {
		if remote == pid() {
			panic!("Sender::<{}>::new() called with process's own pid. A process cannot create a channel to itself.", unsafe{intrinsics::type_name::<T>()});
		}
		let context = REACTOR.read().unwrap();
		if let Some(sender) = channel::Sender::new(
			remote.addr(),
			context.as_ref().unwrap_or_else(|| {
				panic!("You must call init() immediately inside your application's main() function")
			}),
		) {
			Sender(Some(sender), remote)
		} else {
			panic!(
				"Sender::<{}>::new() called for pid {} when a Sender to this pid already exists",
				unsafe { intrinsics::type_name::<T>() },
				remote
			);
		}
	}

	/// Get the pid of the remote end of this Sender
	pub fn remote_pid(&self) -> Pid {
		self.1
	}

	fn async_send<'a>(&'a self) -> Option<impl FnOnce(T) + 'a>
	where
		T: 'static,
	{
		let context = REACTOR.read().unwrap();
		self.0
			.as_ref()
			.unwrap()
			.async_send(BorrowMap::new(context, borrow_unwrap_option))
	}

	/// Blocking send.
	pub fn send(&self, t: T)
	where
		T: 'static,
	{
		self.0.as_ref().unwrap().send(t, &mut || {
			BorrowMap::new(REACTOR.read().unwrap(), borrow_unwrap_option)
		})
	}

	/// [Selectable] send.
	///
	/// This needs to be passed to [`select()`](select) to be executed.
	pub fn selectable_send<'a, F: FnOnce() -> T + 'a>(&'a self, send: F) -> impl Selectable + 'a
	where
		T: 'static,
	{
		self.0.as_ref().unwrap().selectable_send(send)
	}
}

#[doc(hidden)] // noise
impl<T: serde::ser::Serialize> Drop for Sender<T> {
	fn drop(&mut self) {
		let context = REACTOR.read().unwrap();
		self.0.take().unwrap().drop(context.as_ref().unwrap())
	}
}
impl<'a> Write for &'a Sender<u8> {
	#[inline(always)]
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		self.send(buf[0]);
		if buf.len() == 1 {
			return Ok(1);
		}
		for (i, buf) in (1..buf.len()).zip(buf[1..].iter().cloned()) {
			if let Some(send) = self.async_send() {
				send(buf);
			} else {
				return Ok(i);
			}
		}
		Ok(buf.len())
	}

	#[inline(always)]
	fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
		for &byte in buf {
			self.send(byte);
		}
		Ok(())
	}

	#[inline(always)]
	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}
impl Write for Sender<u8> {
	#[inline(always)]
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		(&*self).write(buf)
	}

	#[inline(always)]
	fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
		(&*self).write_all(buf)
	}

	#[inline(always)]
	fn flush(&mut self) -> io::Result<()> {
		(&*self).flush()
	}
}
impl<T: serde::ser::Serialize> fmt::Debug for Sender<T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		self.0.fmt(f)
	}
}
// impl<T: 'static + serde::ser::Serialize> futures::sink::Sink for Sender<Option<T>> {
// 	type SinkError = !;
// 	type SinkItem = T;

// 	fn poll_ready(
// 		self: pin::Pin<&mut Self>, cx: &futures::task::LocalWaker,
// 	) -> futures::task::Poll<Result<(), Self::SinkError>> {
// 		let context = REACTOR.read().unwrap();
// 		self.0
// 			.as_ref()
// 			.unwrap()
// 			.futures_poll_ready(cx, context.as_ref().unwrap())
// 	}

// 	fn start_send(self: pin::Pin<&mut Self>, item: Self::SinkItem) -> Result<(), Self::SinkError> {
// 		let context = REACTOR.read().unwrap();
// 		self.0
// 			.as_ref()
// 			.unwrap()
// 			.futures_start_send(item, context.as_ref().unwrap())
// 	}

// 	fn poll_flush(
// 		self: pin::Pin<&mut Self>, _cx: &futures::task::LocalWaker,
// 	) -> futures::task::Poll<Result<(), Self::SinkError>> {
// 		futures::task::Poll::Ready(Ok(()))
// 	}

// 	fn poll_close(
// 		self: pin::Pin<&mut Self>, cx: &futures::task::LocalWaker,
// 	) -> futures::task::Poll<Result<(), Self::SinkError>> {
// 		let context = REACTOR.read().unwrap();
// 		self.0
// 			.as_ref()
// 			.unwrap()
// 			.futures_poll_close(cx, context.as_ref().unwrap())
// 	}
// }

/// The receiving half of a channel.
///
/// It has a synchronous blocking method [`recv()`](Receiver::recv) and an asynchronous nonblocking method [`selectable_recv()`](Receiver::selectable_recv).
pub struct Receiver<T: serde::de::DeserializeOwned>(Option<channel::Receiver<T>>, Pid);
impl<T: serde::de::DeserializeOwned> Receiver<T> {
	/// Create a new `Receiver<T>` with a remote [Pid]. This method returns instantly.
	pub fn new(remote: Pid) -> Self {
		if remote == pid() {
			panic!("Receiver::<{}>::new() called with process's own pid. A process cannot create a channel to itself.", unsafe{intrinsics::type_name::<T>()});
		}
		let context = REACTOR.read().unwrap();
		if let Some(receiver) = channel::Receiver::new(
			remote.addr(),
			context.as_ref().unwrap_or_else(|| {
				panic!("You must call init() immediately inside your application's main() function")
			}),
		) {
			Receiver(Some(receiver), remote)
		} else {
			panic!(
				"Sender::<{}>::new() called for pid {} when a Sender to this pid already exists",
				unsafe { intrinsics::type_name::<T>() },
				remote
			);
		}
	}

	/// Get the pid of the remote end of this Receiver
	pub fn remote_pid(&self) -> Pid {
		self.1
	}

	fn async_recv<'a>(&'a self) -> Option<impl FnOnce() -> Result<T, ChannelError> + 'a>
	where
		T: 'static,
	{
		let context = REACTOR.read().unwrap();
		self.0
			.as_ref()
			.unwrap()
			.async_recv(BorrowMap::new(context, borrow_unwrap_option))
	}

	/// Blocking receive.
	pub fn recv(&self) -> Result<T, ChannelError>
	where
		T: 'static,
	{
		self.0
			.as_ref()
			.unwrap()
			.recv(&mut || BorrowMap::new(REACTOR.read().unwrap(), borrow_unwrap_option))
	}

	/// [Selectable] receive.
	///
	/// This needs to be passed to [`select()`](select) to be executed.
	pub fn selectable_recv<'a, F: FnOnce(Result<T, ChannelError>) + 'a>(
		&'a self, recv: F,
	) -> impl Selectable + 'a
	where
		T: 'static,
	{
		self.0.as_ref().unwrap().selectable_recv(recv)
	}
}
#[doc(hidden)] // noise
impl<T: serde::de::DeserializeOwned> Drop for Receiver<T> {
	fn drop(&mut self) {
		let context = REACTOR.read().unwrap();
		self.0.take().unwrap().drop(context.as_ref().unwrap())
	}
}
impl<'a> Read for &'a Receiver<u8> {
	#[inline(always)]
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		buf[0] = self.recv().map_err(|e| match e {
			ChannelError::Exited => io::ErrorKind::UnexpectedEof,
			ChannelError::Error => io::ErrorKind::ConnectionReset,
		})?;
		if buf.len() == 1 {
			return Ok(1);
		}
		for (i, buf) in (1..buf.len()).zip(buf[1..].iter_mut()) {
			if let Some(recv) = self.async_recv() {
				if let Ok(t) = recv() {
					*buf = t;
				} else {
					return Ok(i);
				}
			} else {
				return Ok(i);
			}
		}
		Ok(buf.len())
	}

	#[inline(always)]
	fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
		for byte in buf {
			*byte = self.recv().map_err(|e| match e {
				ChannelError::Exited => io::ErrorKind::UnexpectedEof,
				ChannelError::Error => io::ErrorKind::ConnectionReset,
			})?;
		}
		Ok(())
	}

	#[inline(always)]
	unsafe fn initializer(&self) -> io::Initializer {
		io::Initializer::nop()
	}
}
impl Read for Receiver<u8> {
	#[inline(always)]
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		(&*self).read(buf)
	}

	#[inline(always)]
	fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
		(&*self).read_exact(buf)
	}

	#[inline(always)]
	unsafe fn initializer(&self) -> io::Initializer {
		(&&*self).initializer()
	}
}
impl<T: serde::de::DeserializeOwned> fmt::Debug for Receiver<T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		self.0.fmt(f)
	}
}
// impl<T: 'static + serde::de::DeserializeOwned> futures::stream::Stream for Receiver<Option<T>> {
// 	type Item = Result<T, ChannelError>;

// 	fn poll_next(
// 		self: pin::Pin<&mut Self>, cx: &futures::task::LocalWaker,
// 	) -> futures::task::Poll<Option<Self::Item>> {
// 		let context = REACTOR.read().unwrap();
// 		self.0
// 			.as_ref()
// 			.unwrap()
// 			.futures_poll_next(cx, context.as_ref().unwrap())
// 	}
// }

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

/// `select()` lets you block on multiple blocking operations until progress can be made on at least one.
///
/// [`Receiver::selectable_recv()`](Receiver::selectable_recv) and [`Sender::selectable_send()`](Sender::selectable_send) let one create [Selectable] objects, any number of which can be passed to `select()`. `select()` then blocks until at least one is progressable, and then from any that are progressable picks one at random and executes it.
///
/// It returns an iterator of all the [Selectable] objects bar the one that has been executed.
///
/// It is inspired by the `select()` of go, which itself draws from David May's language [occam](https://en.wikipedia.org/wiki/Occam_(programming_language)) and Tony Hoare’s formalisation of [Communicating Sequential Processes](https://en.wikipedia.org/wiki/Communicating_sequential_processes).
pub fn select<'a>(
	select: Vec<Box<Selectable + 'a>>,
) -> impl Iterator<Item = Box<Selectable + 'a>> + 'a {
	channel::select(select, &mut || {
		BorrowMap::new(REACTOR.read().unwrap(), borrow_unwrap_option)
	})
}
/// A thin wrapper around [`select()`](select) that loops until all [Selectable] objects have been executed.
pub fn run<'a>(mut select: Vec<Box<Selectable + 'a>>) {
	while !select.is_empty() {
		select = self::select(select).collect();
	}
}

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

/// Get the [Pid] of the current process
#[inline(always)]
pub fn pid() -> Pid {
	// TODO: panic!("You must call init() immediately inside your application's main() function")
	// TODO: cache
	let listener = unsafe { net::TcpListener::from_raw_fd(LISTENER_FD) };
	let local_addr = listener.local_addr().unwrap();
	let _ = listener.into_raw_fd();
	Pid::new(local_addr.ip(), local_addr.port())
}

/// Get the memory and CPU requirements configured at initialisation of the current process
pub fn resources() -> Resources {
	RESOURCES.read().unwrap().unwrap_or_else(|| {
		panic!("You must call init() immediately inside your application's main() function")
	})
}

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

fn spawn_native(
	resources: Resources, f: serde_closure::FnOnce<(Vec<u8>,), fn((Vec<u8>,), (Pid,))>,
) -> Option<Pid> {
	trace!("spawn_native");
	let argv: Vec<CString> = get_env::args_os()
		.expect("Couldn't get argv")
		.iter()
		.map(|x| CString::new(OsStringExt::into_vec(x.clone())).unwrap())
		.collect(); // argv.split('\0').map(|x|CString::new(x).unwrap()).collect();
	let envp: Vec<(CString, CString)> = get_env::vars_os()
		.expect("Couldn't get envp")
		.iter()
		.map(|&(ref x, ref y)| {
			(
				CString::new(OsStringExt::into_vec(x.clone())).unwrap(),
				CString::new(OsStringExt::into_vec(y.clone())).unwrap(),
			)
		})
		.chain(iter::once((
			CString::new("CONSTELLATION_RESOURCES").unwrap(),
			CString::new(serde_json::to_string(&resources).unwrap()).unwrap(),
		)))
		.collect(); //envp.split('\0').map(|x|{let (a,b) = x.split_at(x.chars().position(|x|x=='=').unwrap_or_else(||panic!("invalid envp {:?}", x)));(CString::new(a).unwrap(),CString::new(&b[1..]).unwrap())}).collect();

	let our_pid = pid();

	let (process_listener, process_id) = native_process_listener();

	let mut spawn_arg: Vec<u8> = Vec::new();
	let bridge_pid: Pid = BRIDGE.read().unwrap().unwrap();
	bincode::serialize_into(&mut spawn_arg, &bridge_pid).unwrap();
	bincode::serialize_into(&mut spawn_arg, &our_pid).unwrap();
	bincode::serialize_into(&mut spawn_arg, &f).unwrap();

	let mut arg = unsafe {
		fs::File::from_raw_fd(memfd_create(&argv[0], false).expect("Failed to memfd_create"))
	};
	// assert_eq!(arg.as_raw_fd(), ARG_FD);
	unistd::ftruncate(arg.as_raw_fd(), spawn_arg.len().try_into().unwrap()).unwrap();
	arg.write_all(&spawn_arg).unwrap();
	let x = unistd::lseek(arg.as_raw_fd(), 0, unistd::Whence::SeekSet).unwrap();
	assert_eq!(x, 0);

	let exe = CString::new(<OsString as OsStringExt>::into_vec(
		exe_path().unwrap().into(),
		// std::env::current_exe().unwrap().into(),
	))
	.unwrap();
	let envp = envp
		.into_iter()
		.map(|(key, value)| {
			CString::new(format!(
				"{}={}",
				key.to_str().unwrap(),
				value.to_str().unwrap()
			))
			.unwrap()
		})
		.collect::<Vec<_>>();

	let _child_pid = match unistd::fork().expect("Fork failed") {
		unistd::ForkResult::Child => {
			// Memory can be in a weird state now. Imagine a thread has just taken out a lock,
			// but we've just forked. Lock still held. Avoid deadlock by doing nothing fancy here.
			// Ideally including malloc.

			// let err = unsafe{libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL)}; assert_eq!(err, 0);
			unsafe {
				let _ = signal::sigaction(
					signal::SIGCHLD,
					&signal::SigAction::new(
						signal::SigHandler::SigDfl,
						signal::SaFlags::empty(),
						signal::SigSet::empty(),
					),
				)
				.unwrap();
			};

			let valgrind_start_fd = if is_valgrind() {
				Some(valgrind_start_fd())
			} else {
				None
			};
			// FdIter uses libc::opendir which mallocs. Underlying syscall is getdents…
			for fd in FdIter::new().unwrap().filter(|&fd| {
				fd >= 3
					&& fd != process_listener
					&& fd != arg.as_raw_fd()
					&& (valgrind_start_fd.is_none() || fd < valgrind_start_fd.unwrap())
			}) {
				unistd::close(fd).unwrap();
			}

			if process_listener != LISTENER_FD {
				move_fd(process_listener, LISTENER_FD, fcntl::OFlag::empty(), true).unwrap();
			}
			if arg.as_raw_fd() != ARG_FD {
				move_fd(arg.as_raw_fd(), ARG_FD, fcntl::OFlag::empty(), true).unwrap();
			}

			if !is_valgrind() {
				unistd::execve(&exe, &argv, &envp).expect("Failed to execve /proc/self/exe"); // or fexecve but on linux that uses proc also
			} else {
				let fd = fcntl::open::<path::PathBuf>(
					&fd_path(valgrind_start_fd.unwrap()).unwrap(),
					fcntl::OFlag::O_RDONLY | fcntl::OFlag::O_CLOEXEC,
					stat::Mode::empty(),
				)
				.unwrap();
				let binary_desired_fd_ = valgrind_start_fd.unwrap() - 1;
				assert!(binary_desired_fd_ > fd);
				move_fd(fd, binary_desired_fd_, fcntl::OFlag::empty(), true).unwrap();
				fexecve(binary_desired_fd_, &argv, &envp)
					.expect("Failed to execve /proc/self/fd/n");
			}
			unreachable!();
		}
		unistd::ForkResult::Parent { child, .. } => child,
	};
	unistd::close(process_listener).unwrap();
	drop(arg);
	let new_pid = Pid::new("127.0.0.1".parse().unwrap(), process_id);
	// BRIDGE.read().unwrap().as_ref().unwrap().0.send(ProcessOutputEvent::Spawn(new_pid)).unwrap();
	{
		let file = unsafe { fs::File::from_raw_fd(MONITOR_FD) };
		bincode::serialize_into(&mut &file, &ProcessOutputEvent::Spawn(new_pid)).unwrap();
		let _ = file.into_raw_fd();
	}
	Some(new_pid)
}

fn spawn_deployed(
	resources: Resources, f: serde_closure::FnOnce<(Vec<u8>,), fn((Vec<u8>,), (Pid,))>,
) -> Option<Pid> {
	trace!("spawn_deployed");
	let stream = unsafe { net::TcpStream::from_raw_fd(SCHEDULER_FD) };
	let (mut stream_read, mut stream_write) =
		(BufferedStream::new(&stream), BufferedStream::new(&stream));
	let mut stream_write_ = stream_write.write();
	let binary = if !is_valgrind() {
		exe().unwrap()
	} else {
		unsafe {
			fs::File::from_raw_fd(
				fcntl::open(
					&fd_path(valgrind_start_fd()).unwrap(),
					fcntl::OFlag::O_RDONLY | fcntl::OFlag::O_CLOEXEC,
					stat::Mode::empty(),
				)
				.unwrap(),
			)
		}
	};
	let len: u64 = binary.metadata().unwrap().len();
	bincode::serialize_into(&mut stream_write_, &resources).unwrap();
	bincode::serialize_into::<_, Vec<OsString>>(
		&mut stream_write_,
		&get_env::args_os().expect("Couldn't get argv"),
	)
	.unwrap();
	bincode::serialize_into::<_, Vec<(OsString, OsString)>>(
		&mut stream_write_,
		&get_env::vars_os().expect("Couldn't get envp"),
	)
	.unwrap();
	bincode::serialize_into(&mut stream_write_, &len).unwrap();
	drop(stream_write_);
	// copy(&mut &binary, &mut stream_write_, len as usize).unwrap();
	copy_sendfile(&binary, &**stream_write.get_ref(), len).unwrap();
	let mut stream_write_ = stream_write.write();
	let mut arg_: Vec<u8> = Vec::new();
	let bridge_pid: Pid = BRIDGE.read().unwrap().unwrap();
	bincode::serialize_into(&mut arg_, &bridge_pid).unwrap();
	bincode::serialize_into(&mut arg_, &pid()).unwrap();
	bincode::serialize_into(&mut arg_, &f).unwrap();
	bincode::serialize_into(&mut stream_write_, &arg_).unwrap();
	drop(stream_write_);
	let pid: Option<Pid> = bincode::deserialize_from(&mut stream_read)
		.map_err(map_bincode_err)
		.unwrap();
	drop(stream_read);
	trace!("{} spawned? {}", self::pid(), pid.unwrap());
	if let Some(pid) = pid {
		let file = unsafe { fs::File::from_raw_fd(MONITOR_FD) };
		bincode::serialize_into(&mut &file, &ProcessOutputEvent::Spawn(pid)).unwrap();
		let _ = file.into_raw_fd();
	}
	let _ = stream.into_raw_fd();
	pid
}

/// Spawn a new process.
///
/// `spawn()` takes 2 arguments:
///  * `resources`: memory and CPU resource requirements of the new process
///  * `start`: the closure to be run in the new process
///
/// `spawn()` returns an Option<Pid>, which contains the [Pid] of the new process.
pub fn spawn<T: FnOnce(Pid) + serde::ser::Serialize + serde::de::DeserializeOwned>(
	resources: Resources, start: T,
) -> Option<Pid> {
	let _scheduler = SCHEDULER.lock().unwrap();
	let deployed = DEPLOYED.read().unwrap().unwrap_or_else(|| {
		panic!("You must call init() immediately inside your application's main() function")
	});
	let arg: Vec<u8> = bincode::serialize(&start).unwrap();
	let start: serde_closure::FnOnce<(Vec<u8>,), fn((Vec<u8>,), (Pid,))> = FnOnce!([arg]move|parent|{
		let arg: Vec<u8> = arg;
		let closure: T = bincode::deserialize(&arg).unwrap();
		closure(parent)
	});
	if !deployed {
		spawn_native(resources, start)
	} else {
		spawn_deployed(resources, start)
	}
}

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

extern "C" fn at_exit() {
	let handle = HANDLE.try_write().unwrap().take().unwrap();
	drop(handle);
	let mut context = REACTOR.write().unwrap();
	drop(context.take().unwrap());
}

#[doc(hidden)]
pub fn bridge_init() -> net::TcpListener {
	const BOUND_FD: Fd = 5; // from fabric
	if is_valgrind() {
		unistd::close(valgrind_start_fd() - 1 - 12).unwrap();
	}
	// init();
	socket::listen(BOUND_FD, 100).unwrap();
	let listener = unsafe { net::TcpListener::from_raw_fd(BOUND_FD) };
	{
		let arg = unsafe { fs::File::from_raw_fd(ARG_FD) };
		let sched_arg: SchedulerArg = bincode::deserialize_from(&mut &arg).unwrap();
		drop(arg);
		let scheduler = net::TcpStream::connect(sched_arg.scheduler)
			.unwrap()
			.into_raw_fd();
		if scheduler != SCHEDULER_FD {
			move_fd(scheduler, SCHEDULER_FD, fcntl::OFlag::empty(), true).unwrap();
		}

		let reactor = channel::Reactor::with_fd(LISTENER_FD);
		*REACTOR.try_write().unwrap() = Some(reactor);
		let handle = channel::Reactor::run(
			|| BorrowMap::new(REACTOR.read().unwrap(), borrow_unwrap_option),
			|&_fd| None,
		);
		*HANDLE.try_write().unwrap() = Some(handle);

		let err = unsafe { libc::atexit(at_exit) };
		assert_eq!(err, 0);
	}
	listener
}

fn native_bridge(format: Format, our_pid: Pid) -> Pid {
	let (bridge_process_listener, bridge_process_id) = native_process_listener();

	// No threads spawned between init and here so we're good
	if let unistd::ForkResult::Parent { .. } = unistd::fork().unwrap() {
		#[cfg(any(target_os = "android", target_os = "linux"))]
		{
			let err = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) };
			assert_eq!(err, 0);
		}
		// trace!("parent");

		move_fd(
			bridge_process_listener,
			LISTENER_FD,
			fcntl::OFlag::empty(),
			false,
		)
		.unwrap();

		let reactor = channel::Reactor::with_fd(LISTENER_FD);
		*REACTOR.try_write().unwrap() = Some(reactor);
		let handle = channel::Reactor::run(
			|| BorrowMap::new(REACTOR.read().unwrap(), borrow_unwrap_option),
			move |&_fd| None,
		);
		*HANDLE.try_write().unwrap() = Some(handle);

		let err = unsafe { libc::atexit(at_exit) };
		assert_eq!(err, 0);

		let x = thread_spawn(String::from("bridge-waitpid"), || {
			loop {
				match wait::waitpid(None, None) {
					Ok(wait::WaitStatus::Exited(_pid, code)) if code == 0 => (), //assert_eq!(pid, child),
					// wait::WaitStatus::Signaled(pid, signal, _) if signal == signal::Signal::SIGKILL => assert_eq!(pid, child),
					Err(nix::Error::Sys(errno::Errno::ECHILD)) => break,
					wait_status => {
						panic!("bad exit: {:?}", wait_status); /*loop {thread::sleep_ms(1000)}*/
					}
				}
			}
		});
		let mut exit_code = ExitStatus::Success;
		let mut formatter = if let Format::Human = format {
			Either::Left(Formatter::new(
				our_pid,
				if atty::is(atty::Stream::Stderr) {
					StyleSupport::EightBit
				} else {
					StyleSupport::None
				},
			))
		} else {
			Either::Right(io::stdout())
		};
		let mut processes = vec![(
			Sender::<ProcessInputEvent>::new(our_pid),
			Receiver::<ProcessOutputEvent>::new(our_pid),
		)];
		while !processes.is_empty() {
			// trace!("select");
			let mut event = None;
			let event_ = &cell::RefCell::new(&mut event);

			let _ = select(
				processes
					.iter()
					.enumerate()
					.map(|(i, &(_, ref receiver))| {
						Box::new(receiver.selectable_recv(
							move |t: Result<ProcessOutputEvent, _>| {
								// trace!("ProcessOutputEvent {}: {:?}", i, t);
								**event_.borrow_mut() = Some((i, t.unwrap()));
							},
						)) as Box<Selectable>
					})
					.collect(),
			);
			// trace!("/select");
			// drop(event_);
			let (i, event): (usize, ProcessOutputEvent) = event.unwrap();
			let pid = processes[i].0.remote_pid();
			let event = match event {
				ProcessOutputEvent::Spawn(new_pid) => {
					processes.push((
						Sender::<ProcessInputEvent>::new(new_pid),
						Receiver::<ProcessOutputEvent>::new(new_pid),
					));
					DeployOutputEvent::Spawn(pid, new_pid)
				}
				ProcessOutputEvent::Output(fd, output) => {
					// sender_.send(OutputEventInt::Output(pid, fd, output)).expect("send failed 1");
					// trace!("output: {:?} {:?}", fd, output);
					// print!("{}", output);
					DeployOutputEvent::Output(pid, fd, output)
				}
				ProcessOutputEvent::Exit(exit_code_) => {
					exit_code += exit_code_;
					let _ = processes.remove(i);
					DeployOutputEvent::Exit(pid, exit_code_)
				}
			};
			match &mut formatter {
				&mut Either::Left(ref mut formatter) => formatter.write(&event),
				&mut Either::Right(ref mut stdout) => {
					serde_json::to_writer(&mut *stdout, &event).unwrap();
					stdout.write_all(b"\n").unwrap()
				}
			}
		}
		x.join().unwrap();
		process::exit(exit_code.into());
	}
	unistd::close(bridge_process_listener).unwrap();
	Pid::new("127.0.0.1".parse().unwrap(), bridge_process_id)
}

fn native_process_listener() -> (Fd, u16) {
	let process_listener = socket(
		socket::AddressFamily::Inet,
		socket::SockType::Stream,
		SockFlag::SOCK_NONBLOCK,
		socket::SockProtocol::Tcp,
	)
	.unwrap();
	socket::setsockopt(process_listener, sockopt::ReuseAddr, &true).unwrap();
	socket::bind(
		process_listener,
		&socket::SockAddr::Inet(socket::InetAddr::from_std(&net::SocketAddr::new(
			"127.0.0.1".parse().unwrap(),
			0,
		))),
	)
	.unwrap();
	socket::setsockopt(process_listener, sockopt::ReusePort, &true).unwrap();
	let process_id =
		if let socket::SockAddr::Inet(inet) = socket::getsockname(process_listener).unwrap() {
			inet.to_std()
		} else {
			panic!()
		};
	assert_eq!(
		process_id.ip(),
		"127.0.0.1".parse::<net::Ipv4Addr>().unwrap()
	);

	(process_listener, process_id.port())
}

fn monitor_process(
	bridge: Pid, deployed: bool,
) -> (channel::SocketForwardee, Fd, Fd, Option<Fd>, Fd) {
	const FORWARD_STDERR: bool = true;

	let (socket_forwarder, socket_forwardee) = channel::socket_forwarder();

	let (monitor_reader, monitor_writer) = unistd::pipe().unwrap(); // unistd::pipe2(fcntl::OFlag::empty())

	let (stdout_reader, stdout_writer) = unistd::pipe().unwrap();
	let (stderr_reader, stderr_writer) = if FORWARD_STDERR {
		let (stderr_reader, stderr_writer) = unistd::pipe().unwrap();
		(Some(stderr_reader), Some(stderr_writer))
	} else {
		(None, None)
	};
	let (stdin_reader, stdin_writer) = unistd::pipe().unwrap();

	let (reader, writer) = unistd::pipe().unwrap(); // unistd::pipe2(fcntl::OFlag::empty())

	// trace!("forking");
	// No threads spawned between init and here so we're good
	if let unistd::ForkResult::Parent { child } = unistd::fork().unwrap() {
		unistd::close(reader).unwrap();
		unistd::close(monitor_writer).unwrap();
		unistd::close(stdout_writer).unwrap();
		if let Some(stderr_writer) = stderr_writer {
			unistd::close(stderr_writer).unwrap();
		}
		unistd::close(stdin_reader).unwrap();
		let (bridge_outbound_sender, bridge_outbound_receiver) =
			mpsc::sync_channel::<ProcessOutputEvent>(0);
		let (bridge_inbound_sender, bridge_inbound_receiver) =
			mpsc::sync_channel::<ProcessInputEvent>(0);
		let stdout_thread = forward_fd(
			libc::STDOUT_FILENO,
			stdout_reader,
			bridge_outbound_sender.clone(),
		);
		let stderr_thread = stderr_reader.map(|stderr_reader| {
			forward_fd(
				libc::STDERR_FILENO,
				stderr_reader,
				bridge_outbound_sender.clone(),
			)
		});
		let _stdin_thread =
			forward_input_fd(libc::STDIN_FILENO, stdin_writer, bridge_inbound_receiver);
		let fd = fcntl::open("/dev/null", fcntl::OFlag::O_RDWR, stat::Mode::empty()).unwrap();
		move_fd(fd, libc::STDIN_FILENO, fcntl::OFlag::empty(), false).unwrap();
		copy_fd(
			libc::STDIN_FILENO,
			libc::STDOUT_FILENO,
			fcntl::OFlag::empty(),
			false,
		)
		.unwrap();
		if FORWARD_STDERR {
			copy_fd(
				libc::STDIN_FILENO,
				libc::STDERR_FILENO,
				fcntl::OFlag::empty(),
				false,
			)
			.unwrap();
		}

		let reactor = channel::Reactor::with_fd(LISTENER_FD);
		*REACTOR.try_write().unwrap() = Some(reactor);
		let handle = channel::Reactor::run(
			|| BorrowMap::new(REACTOR.read().unwrap(), borrow_unwrap_option),
			move |&fd| {
				if let Ok(remote) = socket::getpeername(fd).map(|remote| {
					if let socket::SockAddr::Inet(inet) = remote {
						inet.to_std()
					} else {
						panic!()
					}
				}) {
					if remote == bridge.addr() {
						trace!("{}: {:?} == {:?}", pid(), remote, bridge.addr());
						None
					} else {
						trace!("{}: {:?} != {:?}", pid(), remote, bridge.addr());
						Some(socket_forwarder.clone())
					}
				} else {
					trace!("{}: getpeername failed", pid());
					None
				}
			},
		);
		*HANDLE.try_write().unwrap() = Some(handle);

		let err = unsafe { libc::atexit(at_exit) };
		assert_eq!(err, 0);

		let sender = Sender::<ProcessOutputEvent>::new(bridge);
		let receiver = Receiver::<ProcessInputEvent>::new(bridge);

		let bridge_sender2 = bridge_outbound_sender.clone();
		let x3 = thread_spawn(String::from("monitor-monitorfd-to-channel"), move || {
			let file = unsafe { fs::File::from_raw_fd(monitor_reader) };
			loop {
				let event: Result<ProcessOutputEvent, _> =
					bincode::deserialize_from(&mut &file).map_err(map_bincode_err);
				if event.is_err() {
					break;
				}
				let event = event.unwrap();
				bridge_sender2.send(event).unwrap();
			}
			let _ = file.into_raw_fd();
		});

		let x = thread_spawn(String::from("monitor-channel-to-bridge"), move || {
			loop {
				let event = bridge_outbound_receiver.recv().unwrap();
				sender.send(event.clone());
				if let ProcessOutputEvent::Exit(_) = event {
					// trace!("xxx exit");
					break;
				}
			}
		});
		let _x2 = thread_spawn(String::from("monitor-bridge-to-channel"), move || {
			loop {
				let event: Result<ProcessInputEvent, _> = receiver.recv();
				if event.is_err() {
					break;
				}
				let event = event.unwrap();
				match event {
					ProcessInputEvent::Input(fd, input) => {
						// trace!("xxx INPUT {:?} {}", input, input.len());
						if fd == libc::STDIN_FILENO {
							bridge_inbound_sender
								.send(ProcessInputEvent::Input(fd, input))
								.unwrap();
						} else {
							unimplemented!()
						}
					}
					ProcessInputEvent::Kill => {
						signal::kill(child, signal::Signal::SIGKILL).unwrap_or_else(|e| {
							assert_eq!(e, nix::Error::Sys(errno::Errno::ESRCH))
						});
						break;
					}
				}
			}
		});
		unistd::close(writer).unwrap();

		trace!(
			"PROCESS {}:{}: awaiting exit",
			unistd::getpid(),
			pid().addr().port()
		);
		// trace!("awaiting exit");

		let exit = wait::waitpid(child, None).unwrap();
		trace!(
			"PROCESS {}:{}: exited {:?}",
			unistd::getpid(),
			pid().addr().port(),
			exit
		);
		#[cfg(not(any(
			target_os = "android",
			target_os = "freebsd",
			target_os = "linux",
			target_os = "netbsd",
			target_os = "openbsd"
		)))]
		{
			use std::env;
			if deployed {
				unistd::unlink(&env::current_exe().unwrap()).unwrap();
			}
		}
		#[cfg(any(
			target_os = "android",
			target_os = "freebsd",
			target_os = "linux",
			target_os = "netbsd",
			target_os = "openbsd"
		))]
		{
			let _ = deployed;
		}

		let code = match exit {
			wait::WaitStatus::Exited(pid, code) => {
				assert_eq!(pid, child);
				assert!(0 <= code && code <= i32::from(u8::max_value()));
				ExitStatus::from_unix_status(code.try_into().unwrap())
			}
			wait::WaitStatus::Signaled(pid, signal, _) => {
				assert_eq!(pid, child);
				ExitStatus::from_unix_signal(signal)
			}
			_ => panic!(),
		};
		// trace!("joining stdout_thread");
		stdout_thread.join().unwrap();
		// trace!("joining stderr_thread");
		if FORWARD_STDERR {
			stderr_thread.unwrap().join().unwrap();
		}
		// trace!("joining x3");
		x3.join().unwrap();
		bridge_outbound_sender
			.send(ProcessOutputEvent::Exit(code))
			.unwrap();
		drop(bridge_outbound_sender);
		// trace!("joining x");
		x.join().unwrap();
		// unistd::close(libc::STDIN_FILENO).unwrap();
		// trace!("joining x2");
		// x2.join().unwrap();
		// trace!("joining stdin_thread");
		// stdin_thread.join().unwrap();
		// trace!("exiting");
		// unsafe{libc::_exit(0)};
		process::exit(0);
	}
	unistd::close(monitor_reader).unwrap();
	unistd::close(writer).unwrap();
	unistd::close(stdin_writer).unwrap();
	if FORWARD_STDERR {
		unistd::close(stderr_reader.unwrap()).unwrap();
	}
	unistd::close(stdout_reader).unwrap();
	#[cfg(any(target_os = "android", target_os = "linux"))]
	{
		let err = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
		assert_eq!(err, 0);
	}
	trace!("awaiting ready");
	let err = unistd::read(reader, &mut [0]).unwrap();
	assert_eq!(err, 0);
	unistd::close(reader).unwrap();
	trace!("ready");

	(
		socket_forwardee,
		monitor_writer,
		stdout_writer,
		stderr_writer,
		stdin_reader,
	)
}

/// Initialise the [deploy](self) runtime. This must be called immediately inside your application's `main()` function.
///
/// The `resources` argument describes memory and CPU requirements for the initial process.
pub fn init(resources: Resources) {
	if is_valgrind() {
		let _ = unistd::close(valgrind_start_fd() - 1 - 12); // close non CLOEXEC'd fd of this binary
	}
	let envs = Envs::from(&get_env::vars_os().expect("Couldn't get envp"));
	let version = envs
		.version
		.map_or(false, |x| x.expect("CONSTELLATION_VERSION must be 0 or 1"));
	let recce = envs
		.recce
		.map_or(false, |x| x.expect("CONSTELLATION_RECCE must be 0 or 1"));
	let format = envs.format.map_or(Format::Human, |x| {
		x.expect("CONSTELLATION_FORMAT must be json or human")
	});
	let deployed = envs.deploy == Some(Some(Deploy::Fabric));
	if version {
		assert!(!recce);
		write!(io::stdout(), "deploy-lib {}", env!("CARGO_PKG_VERSION")).unwrap();
		process::exit(0);
	}
	if recce {
		let file = unsafe { fs::File::from_raw_fd(3) };
		bincode::serialize_into(&file, &resources).unwrap();
		drop(file);
		process::exit(0);
	}
	let (subprocess, resources, argument, bridge, scheduler) = {
		if !deployed {
			if envs.resources.is_none() {
				(false, resources, vec![], None, None)
			} else {
				let arg = unsafe { fs::File::from_raw_fd(ARG_FD) };
				let bridge = bincode::deserialize_from(&mut &arg)
					.map_err(map_bincode_err)
					.unwrap();
				let mut prog_arg = Vec::new();
				let _ = (&arg).read_to_end(&mut prog_arg).unwrap();
				(
					true,
					envs.resources.unwrap().unwrap(),
					prog_arg,
					Some(bridge),
					None,
				)
			}
		} else {
			let arg = unsafe { fs::File::from_raw_fd(ARG_FD) };
			let sched_arg: SchedulerArg = bincode::deserialize_from(&mut &arg).unwrap();
			let bridge: Pid = bincode::deserialize_from(&mut &arg).unwrap();
			let mut prog_arg = Vec::new();
			let _ = (&arg).read_to_end(&mut prog_arg).unwrap();
			let subprocess = !prog_arg.is_empty();
			if !subprocess {
				assert_eq!(resources, envs.resources.unwrap().unwrap());
			}
			(
				subprocess,
				envs.resources.unwrap().unwrap(),
				prog_arg,
				Some(bridge),
				Some(sched_arg.scheduler),
			)
		}
	};

	trace!(
		"PROCESS {}:{}: start setup; pid: {}",
		unistd::getpid(),
		pid().addr().port(),
		pid()
	);

	let bridge = bridge.unwrap_or_else(|| {
		// We're in native topprocess
		let (our_process_listener, our_process_id) = native_process_listener();
		if our_process_listener != LISTENER_FD {
			move_fd(
				our_process_listener,
				LISTENER_FD,
				fcntl::OFlag::empty(),
				true,
			)
			.unwrap();
		}
		let our_pid = Pid::new("127.0.0.1".parse().unwrap(), our_process_id);
		assert_eq!(our_pid, pid());
		native_bridge(format, our_pid)
		// let err = unsafe{libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL)}; assert_eq!(err, 0);
	});

	*DEPLOYED.write().unwrap() = Some(deployed);
	*RESOURCES.write().unwrap() = Some(resources);
	*BRIDGE.write().unwrap() = Some(bridge);

	let fd = fcntl::open("/dev/null", fcntl::OFlag::O_RDWR, stat::Mode::empty()).unwrap();
	if fd != SCHEDULER_FD {
		move_fd(fd, SCHEDULER_FD, fcntl::OFlag::empty(), true).unwrap();
	}
	copy_fd(SCHEDULER_FD, MONITOR_FD, fcntl::OFlag::empty(), true).unwrap();

	let (socket_forwardee, monitor_writer, stdout_writer, stderr_writer, stdin_reader) =
		monitor_process(bridge, deployed);
	assert_ne!(monitor_writer, MONITOR_FD);
	move_fd(monitor_writer, MONITOR_FD, fcntl::OFlag::empty(), false).unwrap();
	move_fd(
		stdout_writer,
		libc::STDOUT_FILENO,
		fcntl::OFlag::empty(),
		false,
	)
	.unwrap();
	if let Some(stderr_writer) = stderr_writer {
		move_fd(
			stderr_writer,
			libc::STDERR_FILENO,
			fcntl::OFlag::empty(),
			false,
		)
		.unwrap();
	}
	move_fd(
		stdin_reader,
		libc::STDIN_FILENO,
		fcntl::OFlag::empty(),
		false,
	)
	.unwrap();

	if deployed {
		let scheduler = net::TcpStream::connect(scheduler.unwrap())
			.unwrap()
			.into_raw_fd();
		assert_ne!(scheduler, SCHEDULER_FD);
		move_fd(scheduler, SCHEDULER_FD, fcntl::OFlag::empty(), false).unwrap();
	}

	let reactor = channel::Reactor::with_forwardee(socket_forwardee, pid().addr());
	*REACTOR.try_write().unwrap() = Some(reactor);
	let handle = channel::Reactor::run(
		|| BorrowMap::new(REACTOR.read().unwrap(), borrow_unwrap_option),
		|&_fd| None,
	);
	*HANDLE.try_write().unwrap() = Some(handle);

	let err = unsafe { libc::atexit(at_exit) };
	assert_eq!(err, 0);

	unsafe {
		let _ = signal::sigaction(
			signal::SIGCHLD,
			&signal::SigAction::new(
				signal::SigHandler::SigIgn,
				signal::SaFlags::empty(),
				signal::SigSet::empty(),
			),
		)
		.unwrap();
	};

	trace!(
		"PROCESS {}:{}: done setup; pid: {}; bridge: {:?}",
		unistd::getppid(),
		pid().addr().port(),
		pid(),
		bridge
	);

	if !subprocess {
		return;
	} else {
		let (start, parent) = {
			let mut argument = io::Cursor::new(&argument);
			let parent: Pid = bincode::deserialize_from(&mut argument)
				.map_err(map_bincode_err)
				.unwrap();
			let start: serde_closure::FnOnce<(Vec<u8>,), fn((Vec<u8>,), (Pid,))> =
				bincode::deserialize_from(&mut argument)
					.map_err(map_bincode_err)
					.unwrap();
			(start, parent)
		};
		start(parent);
		process::exit(0);
	}
}

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

fn forward_fd(
	fd: Fd, reader: Fd, bridge_sender: mpsc::SyncSender<ProcessOutputEvent>,
) -> thread::JoinHandle<()> {
	thread_spawn(String::from("monitor-forward_fd"), move || {
		let reader = unsafe { fs::File::from_raw_fd(reader) };
		let _ = fcntl::fcntl(reader.as_raw_fd(), fcntl::FcntlArg::F_GETFD).unwrap();
		loop {
			let mut buf: [u8; 1024] = unsafe { mem::uninitialized() };
			let n = (&reader).read(&mut buf).unwrap();
			if n > 0 {
				bridge_sender
					.send(ProcessOutputEvent::Output(fd, buf[..n].to_owned()))
					.unwrap();
			} else {
				drop(reader);
				bridge_sender
					.send(ProcessOutputEvent::Output(fd, Vec::new()))
					.unwrap();
				break;
			}
		}
	})
}

fn forward_input_fd(
	fd: Fd, writer: Fd, receiver: mpsc::Receiver<ProcessInputEvent>,
) -> thread::JoinHandle<()> {
	thread_spawn(String::from("monitor-forward_input_fd"), move || {
		let writer = unsafe { fs::File::from_raw_fd(writer) };
		let _ = fcntl::fcntl(writer.as_raw_fd(), fcntl::FcntlArg::F_GETFD).unwrap();
		for input in receiver {
			match input {
				ProcessInputEvent::Input(fd_, ref input) if fd_ == fd => {
					if !input.is_empty() {
						if (&writer).write_all(input).is_err() {
							drop(writer);
							break;
						}
					} else {
						drop(writer);
						break;
					}
				}
				_ => unreachable!(),
			}
		}
	})
}

fn move_fd(
	oldfd: Fd, newfd: Fd, flags: fcntl::OFlag, allow_nonexistent: bool,
) -> Result<(), nix::Error> {
	if !allow_nonexistent {
		let _ = fcntl::fcntl(newfd, fcntl::FcntlArg::F_GETFD).unwrap();
	}
	palaver::dup_to(oldfd, newfd, flags).and_then(|()| unistd::close(oldfd))
}
fn copy_fd(
	oldfd: Fd, newfd: Fd, flags: fcntl::OFlag, allow_nonexistent: bool,
) -> Result<(), nix::Error> {
	if !allow_nonexistent {
		let _ = fcntl::fcntl(newfd, fcntl::FcntlArg::F_GETFD).unwrap();
	}
	palaver::dup_to(oldfd, newfd, flags)
}

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

struct BorrowMap<T, F: Fn(&T) -> &T1, T1>(T, F, marker::PhantomData<fn() -> T1>);
impl<T, F: Fn(&T) -> &T1, T1> BorrowMap<T, F, T1> {
	fn new(t: T, f: F) -> Self {
		BorrowMap(t, f, marker::PhantomData)
	}
}
impl<T, F: Fn(&T) -> &T1, T1> borrow::Borrow<T1> for BorrowMap<T, F, T1> {
	fn borrow(&self) -> &T1 {
		self.1(&self.0)
	}
}
fn borrow_unwrap_option<T: ops::Deref<Target = Option<T1>>, T1>(x: &T) -> &T1 {
	x.as_ref().unwrap()
}
