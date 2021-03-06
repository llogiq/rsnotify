#![allow(non_upper_case_globals, dead_code)]
extern crate fsevent as fse;

use fsevent_sys::core_foundation as cf;
use fsevent_sys::fsevent as fs;
use std::slice;
use std::mem::transmute;
use std::slice::from_raw_parts_mut;
use std::str::from_utf8;
use std::ffi::CStr;
use std::convert::AsRef;
use std::thread;

use std::sync::mpsc::{channel, Sender, Receiver};
use super::{Error, Event, op, Watcher};
use std::path::{Path, PathBuf};
use libc;

pub struct FsEventWatcher {
  paths: cf::CFMutableArrayRef,
  since_when: fs::FSEventStreamEventId,
  latency: cf::CFTimeInterval,
  flags: fs::FSEventStreamCreateFlags,
  sender: Sender<Event>,
  runloop: Option<usize>,
  context: Option<Box<StreamContextInfo>>,
}

fn translate_flags(flags: fse::StreamFlags) -> op::Op {
  let mut ret = op::Op::empty();
  if flags.contains(fse::ITEM_XATTR_MOD) {
    ret.insert(op::CHMOD);
  }
  if flags.contains(fse::ITEM_CREATED) {
    ret.insert(op::CREATE);
  }
  if flags.contains(fse::ITEM_REMOVED) {
    ret.insert(op::REMOVE);
  }
  if flags.contains(fse::ITEM_RENAMED) {
    ret.insert(op::RENAME);
  }
  if flags.contains(fse::ITEM_MODIFIED)  {
    ret.insert(op::WRITE);
  }
  ret
}

struct StreamContextInfo {
  sender: Sender<Event>,
  done:  Receiver<()>
}

impl FsEventWatcher {
  #[inline]
  pub fn is_running(&self) -> bool {
    self.runloop.is_some()
  }

  pub fn stop(&mut self) {
    if !self.is_running() {
      return;
    }

    if let Some(runloop) = self.runloop {
      unsafe {
        let runloop = runloop as *mut libc::c_void;
        cf::CFRunLoopStop(runloop);
      }
    }

    self.runloop = None;
    if let Some(ref context_info) = self.context {
      // sync done channel
      match context_info.done.recv() {
        Ok(()) => (),
        Err(_) => panic!("the runloop may not be finished!"),
      }
    }

    self.context = None;
  }

  fn remove_path(&mut self, source: &str) {
    unsafe {
      let cf_path = cf::str_path_to_cfstring_ref(source);

      for idx in 0 .. cf::CFArrayGetCount(self.paths) {
        let item = cf::CFArrayGetValueAtIndex(self.paths, idx);
        if cf::CFStringCompare(item, cf_path, cf::kCFCompareCaseInsensitive) == cf::kCFCompareEqualTo {
          cf::CFArrayRemoveValueAtIndex(self.paths, idx);
        }
      }
    }
  }

  // https://github.com/thibaudgg/rb-fsevent/blob/master/ext/fsevent_watch/main.c
  fn append_path(&mut self, source: &str) {
    unsafe {
      let cf_path = cf::str_path_to_cfstring_ref(source);
      cf::CFArrayAppendValue(self.paths, cf_path);
      cf::CFRelease(cf_path);
    }
  }

  pub fn run(&mut self) -> Result<(), Error> {
    if unsafe { cf::CFArrayGetCount(self.paths) } == 0 {
      return Err(Error::PathNotFound);
    }

    // done channel is used to sync quit status of runloop thread
    let (done_tx, done_rx) = channel();

    let info = StreamContextInfo {
      sender: self.sender.clone(),
      done: done_rx
    };

    self.context = Some(Box::new(info));

    let stream_context = fs::FSEventStreamContext{
      version: 0,
      info: unsafe { transmute(self.context.as_ref().map(|ctx| & **ctx)) },
      retain: cf::NULL,
      copy_description: cf::NULL };

    let cb = callback as *mut _;
    let stream = unsafe { fs::FSEventStreamCreate(cf::kCFAllocatorDefault,
                                                  cb,
                                                  &stream_context,
                                                  self.paths,
                                                  self.since_when,
                                                  self.latency,
                                                  self.flags) };

    // move into thread
    let dummy = stream as usize;
    // channel to pass runloop around
    let (rl_tx, rl_rx) = channel();

    thread::spawn(move || {
      let stream = dummy as *mut libc::c_void;
      unsafe {
        let cur_runloop = cf::CFRunLoopGetCurrent();

        fs::FSEventStreamScheduleWithRunLoop(stream, cur_runloop, cf::kCFRunLoopDefaultMode);
        fs::FSEventStreamStart(stream);

        // the calling to CFRunLoopRun will be terminated by CFRunLoopStop call in drop()
        rl_tx.send(cur_runloop  as *mut libc::c_void as usize).ok().expect("Unable to send runloop to watcher");
        cf::CFRunLoopRun();
        fs::FSEventStreamStop(stream);
        fs::FSEventStreamInvalidate(stream);
        fs::FSEventStreamRelease(stream);
      }
      done_tx.send(()).ok().expect("error while signal run loop is done");
    });
    // block until runloop has been set
    self.runloop = Some(rl_rx.recv().unwrap());

    Ok(())
  }
}

#[allow(unused_variables)]
pub unsafe extern "C" fn callback(
  stream_ref: fs::FSEventStreamRef,
  info: *mut libc::c_void,
  num_events: libc::size_t,                // size_t numEvents
  event_paths: *const *const libc::c_char, // void *eventPaths
  event_flags: *mut libc::c_void,          // const FSEventStreamEventFlags eventFlags[]
  event_ids: *mut libc::c_void,            // const FSEventStreamEventId eventIds[]
  ) {
  let num = num_events as usize;
  let e_ptr = event_flags as *mut u32;
  let i_ptr = event_ids as *mut u64;
  let info = transmute::<_, *const StreamContextInfo>(info);

  let paths: &[*const libc::c_char] = transmute(slice::from_raw_parts(event_paths, num));
  let flags = slice::from_raw_parts_mut(e_ptr, num);
  let ids = slice::from_raw_parts_mut(i_ptr, num);

  for p in (0..num) {
    let i = CStr::from_ptr(paths[p]).to_bytes();
    let flag = fse::StreamFlags::from_bits(flags[p] as u32)
      .expect(format!("Unable to decode StreamFlags: {}", flags[p] as u32).as_ref());

    let path = PathBuf::from(from_utf8(i).ok().expect("Invalid UTF8 string."));
    let event = Event{op: Ok(translate_flags(flag)), path: Some(path)};

    (*info).sender.send(event).ok().expect("error while sending event");
  }
}


impl Watcher for FsEventWatcher {
  fn new(tx: Sender<Event>) -> Result<FsEventWatcher, Error> {
    Ok(FsEventWatcher {
      paths: unsafe { cf::CFArrayCreateMutable(cf::kCFAllocatorDefault, 0, &cf::kCFTypeArrayCallBacks) },
      since_when: fs::kFSEventStreamEventIdSinceNow,
      latency: 0.0,
      flags: fs::kFSEventStreamCreateFlagFileEvents | fs::kFSEventStreamCreateFlagNoDefer,
      sender: tx,
      runloop: None,
      context: None,
    })
  }

  fn watch<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
    self.stop();
    self.append_path(&path.as_ref().to_str().unwrap());
    self.run()
  }

  fn unwatch<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
    self.stop();
    self.remove_path(&path.as_ref().to_str().unwrap());
    // ignore return error: may be empty path list
    let _ = self.run();
    Ok(())
  }
}

impl Drop for FsEventWatcher {
  fn drop(&mut self) {
    self.stop();
    unsafe {
      cf::CFRelease(self.paths);
    }
  }
}



#[test]
fn test_fsevent_watcher_drop() {
  use super::*;
  let (tx, rx) = channel();

  {
    let mut watcher: RecommendedWatcher = Watcher::new(tx).unwrap();
    watcher.watch("../../").unwrap();
    thread::sleep_ms(2_000);
    println!("is running -> {}", watcher.is_running());

    thread::sleep_ms(1_000);
    watcher.unwatch("../..").unwrap();
    println!("is running -> {}", watcher.is_running());
  }

  thread::sleep_ms(1_000);

  // if drop() works, this loop will quit after all Sender freed
  // otherwise will block forever
  for e in rx.iter() {
      println!("debug => {:?} {:?}", e.op.map(|e| e.bits()).unwrap_or(0), e.path);
  }

  println!("in test: {} works", file!());
}
