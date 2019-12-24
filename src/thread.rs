use crate::machine;
use crate::println;
use crate::Stack;
use alloc::boxed::Box;

use crate::smp;
use alloc::collections::VecDeque;
use crate::config::CONFIG;
use core::mem::MaybeUninit;
use spin::Mutex;
use core::borrow::BorrowMut;

lazy_static! {
    pub static ref READY: Mutex<VecDeque<Box<dyn TCB>>> = spin::Mutex::new(VecDeque::new());

    /// Invariant: When Active[i] == None, core i is guaranteed not to context switch due to a timer interrupt
    pub static ref ACTIVE: Mutex<[Option<Box<dyn TCB>>; 16]> = {
        let mut active: [MaybeUninit<Option<Box<dyn TCB>>>; 16] =
            unsafe { MaybeUninit::uninit().assume_init() };
        for i in 0..16 {
            active[i] = MaybeUninit::new(Some(Box::new(BootstrapTCB::new())));
        }
        Mutex::new(unsafe { core::mem::transmute::<_, [Option<Box<dyn TCB>>; 16]>(active) })
    };
}

lazy_static! {
    pub static ref CLEANUP: [Mutex<Box<TaskHolder>>; 16] = {
        let mut cleanup: [MaybeUninit<Mutex<Box<TaskHolder>>>; 16] =
            unsafe { MaybeUninit::uninit().assume_init() };
        for i in 0..16 {
            cleanup[i] = MaybeUninit::new(Mutex::new(box TaskHolder::new()));
        }
        unsafe { core::mem::transmute::<_, [Mutex<Box<TaskHolder>>; 16]>(cleanup) }
    };
}


/*
pub fn get_active() -> &'static dyn TCB {
    let was = machine::disable();
    let active: &'static dyn TCB = ACTIVE.lock()[smp::me() as usize].borrow_mut();
    machine::enable(was);
    active
}
*/
pub fn swap_active(swap_to: Option<Box<dyn TCB>>) -> Option<Box<dyn TCB>> {
    let was = machine::disable();
    let mut result = swap_to;
    core::mem::swap(&mut result, &mut ACTIVE.lock()[smp::me() as usize]);
    machine::enable(was);
    result
}


pub trait TCB: core::marker::Send + core::marker::Sync {
    fn get_info(&mut self) -> *mut TCBInfo;
    fn set_leave_me_alone(&mut self, flag: bool);
    fn get_leave_me_alone(&mut self) -> bool;
    fn get_work(&mut self) -> TaskHolder;
}

#[repr(C)]
struct BootstrapTCB {
    tcb_info: TCBInfo,
    stack_frame_start: Option<usize>,
}

impl BootstrapTCB {
    pub fn new() -> BootstrapTCB {
        BootstrapTCB {
            tcb_info: TCBInfo::new(0),
            stack_frame_start: None,
        }
    }
}

impl TCB for BootstrapTCB {
    fn get_info(&mut self) -> *mut TCBInfo {
        &mut self.tcb_info as *mut TCBInfo
    }

    fn get_leave_me_alone(&mut self) -> bool {
        self.tcb_info.leave_me_alone
    }

    fn set_leave_me_alone(&mut self, flag: bool) {
        self.tcb_info.leave_me_alone = flag;
    }

    fn get_work(&mut self) -> TaskHolder {
        panic!("BootstrapTCB has no work to do!");
    }
}


#[repr(C)]
pub struct TCBImpl<T: 'static + Fn() + core::marker::Send + core::marker::Sync> {
    tcb_info: TCBInfo,
    stack: Box<Stack>,
    work: Option<Box<T>>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TCBInfo {
    stack_pointer: usize,
    leave_me_alone: bool,
}

impl TCBInfo {
    pub fn new(stack_pointer: usize) -> TCBInfo {
        TCBInfo {
            stack_pointer: stack_pointer,
            leave_me_alone: false,
        }
    }
}

impl<T: 'static + Fn() + core::marker::Send + core::marker::Sync> TCBImpl<T> {
    const NUM_CALLEE_SAVED: usize = 6;

    pub fn new(work: T) -> TCBImpl<T> {
        let mut stack = box Stack::new();
        let end_of_stack = 511;
        stack.stack[end_of_stack] = thread_entry_point as *const () as u64;
        let index: usize = end_of_stack - TCBImpl::<T>::NUM_CALLEE_SAVED - 1;
        stack.stack[index] = 0; // Flags
        stack.stack[index - 1] = 0; // CR2
        let stack_ptr = Box::into_raw(stack);
        let stack_ptr_as_usize = stack_ptr as usize;
        println!(
            "loaded return at 0x{:x}",
            stack_ptr_as_usize + (end_of_stack * core::mem::size_of::<usize>())
        );
        let x = stack_ptr_as_usize + ((index - 1) * core::mem::size_of::<usize>());
        println!("initial rsp 0x{:x}", x);
        let tcb_info = TCBInfo::new(x);
        stack = unsafe { Box::from_raw(stack_ptr) };
        TCBImpl {
            tcb_info: tcb_info,
            stack: stack,
            work: Some(Box::new(work)),
        }
    }
}

impl<T: 'static + Fn() + core::marker::Send + core::marker::Sync> TCB for TCBImpl<T> {
    fn get_info(&mut self) -> *mut TCBInfo {
        &mut self.tcb_info as *mut TCBInfo
    }

    fn get_leave_me_alone(&mut self) -> bool {
        self.tcb_info.leave_me_alone
    }

    fn set_leave_me_alone(&mut self, flag: bool) {
        self.tcb_info.leave_me_alone = flag;
    }

    fn get_work(&mut self) -> TaskHolder {
        let mut work = None;
        core::mem::swap(&mut work, &mut self.work);
        let mut task_holder = TaskHolder::new();
        match work {
            Some(mut task) => task_holder.add_task(task),
            None => panic!("TCBImpl had no work!")
        }
        task_holder
    }
}


type Cleanup = FnOnce() + core::marker::Send + core::marker::Sync;

/// Holds tasks to perform after context-switching
/// No mutual exclusion needed as this is a per-core data structure
pub struct TaskHolder {
    tasks: VecDeque<Box<Cleanup>>,
}

impl TaskHolder {
    pub fn new() -> TaskHolder {
        TaskHolder {tasks: VecDeque::new()}
    }
    pub fn add_task(&mut self, task: Box<Cleanup>) {
        self.tasks.push_back(task);
    }
    pub fn get_task(&mut self) -> Option<Box<Cleanup>> {
        self.tasks.pop_front()
    }

    pub fn run_task(&mut self) {
        let task: Box<Cleanup> = match self.tasks.pop_front() {
            Some(task) => task,
            None => panic!("No task available!")
        };
        task();
    }
}

#[no_mangle]
pub extern "C" fn thread_entry_point() -> ! {
    println!("thread arrived at entry point with rsp {:x}", unsafe {
        machine::get_rsp()
    });
    cleanup();
    let was = machine::disable();
    let mut active = match swap_active(None) {
        Some(active) => active,
        None => panic!("No thread available in thread entry point"),
    };
    let task_holder = &mut active.get_work();
    swap_active(Some(active));
    machine::enable(was);
    task_holder.run_task();
    println!("thread finished work");
    loop {}
}

pub fn init() {
    println!("initializing threads...");
    lazy_static::initialize(&READY);
    lazy_static::initialize(&ACTIVE);
    lazy_static::initialize(&CLEANUP);
    println!("threads initialized");
}

pub fn surrender() {
    surrender_help(true);
}

pub fn stop() {
    surrender_help(false);
}

/// Yield is a reserved word in Rust, so we use a synonym
fn surrender_help(run_again: bool) {
    //println!("in surrender help");
    // If there's no active thread, return as we are currently surrendering
    let mut current_thread: Box<dyn TCB> = match swap_active(None) {
        Some(mut tcb) => {tcb},
        None => return
    };
    // Don't need to disable interrupts, as we will run on this core until we context switch
    let me = smp::me() as usize;
    //println!("me was {}", me);
    let current_thread_info = current_thread.get_info();
    if (run_again) {
        let add_to_ready = move || {
            READY.lock().push_back(current_thread);
        };
        CLEANUP[me].lock().add_task(Box::new(add_to_ready));
    } else {
        let drop_current = move || {
            let x = current_thread;
        };
        CLEANUP[me].lock().add_task(Box::new(drop_current));
    }
    block(current_thread_info);
}

fn block(current_thread_info: *mut TCBInfo) {
    // Find something to switch to
    let mut next_thread = match READY.lock().pop_front() {
        Some(mut tcb) => tcb,
        None => {
            // Implementation Note: Potentially a trade off to switch to something that switches back,
            // but most of the time, there should be something in the ready q
            Box::new(TCBImpl::new(|| {stop()}))
        }
    };
    let next_thread_info = next_thread.get_info();
    let assert_as_active = move || {
        // The next thread will now assert itself as the active thread
        swap_active(Some(next_thread));
    };
    CLEANUP[smp::me() as usize].lock().add_task(Box::new(assert_as_active));
    unsafe {
        machine::context_switch(current_thread_info, next_thread_info)
    }
    cleanup();
}

fn cleanup() {
    let was = machine::disable();
    let me = smp::me() as usize;
    let mut cleanup_work = CLEANUP[me].lock();
    machine::enable(was);
    loop {
        match cleanup_work.get_task() {
            Some(work) => work(),
            None => break
        }
    }
}

pub fn schedule(tcb: Box<dyn TCB>) {
    READY.lock().push_back(tcb);
}

pub fn surrender_test() {
    let mut test1 = Box::new(TCBImpl::new(|| ()));
    println!("{} in surrender after heap allocation", smp::me());
    let mut test2 = Box::new(TCBImpl::new(|| ()));
    println!("attempting to context switch");
    let x = test2.get_info();
    unsafe {
        println!("switching to rsp {:x}", unsafe { *(x as *mut usize) });
    }
    unsafe {
        machine::context_switch(test1.get_info(), test2.get_info());
    }
}
