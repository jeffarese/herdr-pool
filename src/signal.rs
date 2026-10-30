//! Ctrl-C, a closed terminal or a kill while shards run: note it, so the main
//! thread can end what it started before leaving. Lines to hc run in their
//! own process group, so the terminal's Ctrl-C reaches only us.

use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static STOPS: AtomicUsize = AtomicUsize::new(0);
static HIDDEN: AtomicBool = AtomicBool::new(false);
/// A question has the terminal raw; SAVED is how it was before.
static RAW: AtomicBool = AtomicBool::new(false);
static mut SAVED: MaybeUninit<libc::termios> = MaybeUninit::uninit();

extern "C" fn note(_: libc::c_int) {
    STOPS.fetch_add(1, Ordering::SeqCst);
}

/// Every other command dies of the signal as before, but not with the
/// cursor hidden (a spinner was turning): that would outlive us in the
/// person's shell.
extern "C" fn leave(sig: libc::c_int) {
    if RAW.load(Ordering::SeqCst) {
        // SAFETY: tcsetattr is async-signal-safe, and SAVED was written
        // before RAW was set.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, std::ptr::addr_of!(SAVED).cast()) };
    }
    if HIDDEN.load(Ordering::SeqCst) {
        let show = b"\r\x1b[2K\x1b[?25h";
        // SAFETY: write and signal/raise are async-signal-safe.
        unsafe { libc::write(2, show.as_ptr().cast(), show.len()) };
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn handle(f: extern "C" fn(libc::c_int)) {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handlers only touch atomics and async-signal-safe calls.
        unsafe { libc::signal(sig, f as libc::sighandler_t) };
    }
}

/// For every command, at the start: see `leave`.
pub fn guard() {
    handle(leave);
}

/// From here a signal is only noted: `test` ends what it started first.
pub fn install() {
    handle(note);
}

/// Raw mode outlives a process killed in it (no echo, no line endings), so
/// a question keeps the terminal's settings here for `leave` to put back.
pub fn raw(on: bool) {
    if !on {
        RAW.store(false, Ordering::SeqCst);
        return;
    }
    let mut t = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr fills t when it returns 0; SAVED is written before
    // RAW says it may be read.
    unsafe {
        if libc::tcgetattr(0, t.as_mut_ptr()) == 0 {
            std::ptr::addr_of_mut!(SAVED).write(t);
            RAW.store(true, Ordering::SeqCst);
        }
    }
}

pub fn cursor_hidden(hidden: bool) {
    HIDDEN.store(hidden, Ordering::SeqCst);
}

pub fn stopped() -> bool {
    times() > 0
}

/// How many times: a second Ctrl-C means now.
pub fn times() -> usize {
    STOPS.load(Ordering::SeqCst)
}
