use core::{
    borrow::{Borrow, BorrowMut},
    cell::UnsafeCell,
    cmp,
    fmt::{self, Debug},
    marker::{PhantomData, PhantomPinned},
    mem::MaybeUninit,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize},
    task::Waker,
};

use atomex::{
    x_deps::funty,
    AtomicCount, AtomexPtrOwned, CmpxchResult, PhantomAtomicPtr,
    StrictOrderings, TrAtomicFlags, TrCmpxchOrderings,
};

use super::{RingBuffer, RxError, TxError, Dual};

pub(super) type FnCheckState<O> = fn(&RwState<O>, usize) -> bool;
pub(super) type AtomicDemandPtr<O> = AtomexPtrOwned<Demand<O>, O>;

#[derive(Debug)]
pub(super) struct CheckStateFn<O>(FnCheckState<O>)
where
    O: TrCmpxchOrderings;

impl<O> CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{
    pub const fn new(fp: FnCheckState<O>) -> Self {
        CheckStateFn(fp)
    }
}

impl<O> FnOnce<(&RwState<O>, usize,)> for CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{
    type Output = bool;

    extern "rust-call" fn call_once(
        self,
        args: (&RwState<O>, usize,),
    ) -> Self::Output {
        let f = self.0;
        f(args.0, args.1)
    }
}

impl<O> FnMut<(&RwState<O>, usize,)> for CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{
    extern "rust-call" fn call_mut(
        &mut self,
        args: (&RwState<O>, usize,),
    ) -> Self::Output {
        let f = self.0;
        f(args.0, args.1)
    }
}

unsafe impl<O> Send for CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{}

unsafe impl<O> Sync for CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{}

#[derive(Debug)]
pub(super) struct Demand<O>
where
    O: TrCmpxchOrderings,
{
    count_: usize,
    check_: CheckStateFn<O>,
    waker_: Option<Waker>,
}

impl<O> Demand<O>
where
    O: TrCmpxchOrderings,
{
    pub const DEFAULT_PEEK_COUNT: usize = 1;

    pub const fn new(
        count: usize,
        check: FnCheckState<O>,
    ) -> Self {
        Demand {
            count_: count,
            check_: CheckStateFn::new(check),
            waker_: Option::None,
        }
    }

    pub fn check_state(
        &mut self,
        rw_state: &RwState<O>,
    ) -> bool {
        let f = &mut self.check_;
        f(rw_state, self.count_)
    }

    pub fn try_init_waker(
        &mut self,
        get_waker: impl FnOnce() -> Waker,
    ) -> Result<&Waker, &Waker> {
        let opt = &mut self.waker_;
        if let Option::Some(existing) = opt {
            Result::Err(existing)
        } else {
            *opt = Option::Some(get_waker());
            Result::Ok(opt.as_ref().unwrap())
        }
    }

    pub fn try_take_waker(&mut self) -> Option<Waker> {
        self.waker_.take()
    }

    pub fn producer_check(
        state: &RwState<O>,
        demanded: usize,
    ) -> bool {
        let i = state.load_state();
        let a = cmp::min(state.capacity() >> 1, demanded >> 1);
        // #[cfg(test)]
        // log::trace!("[Demand::producer_check] {i:?}");
        i.wl > a
    }

    pub fn consumer_check(
        state: &RwState<O>,
        demanded: usize,
    ) -> bool {
        let i = state.load_state();
        let a = cmp::min(state.capacity() >> 1, demanded >> 1);
        // #[cfg(test)]
        // log::trace!("[Demand::consumer_check] {i:?}");
        i.rl > a
    }
}

pub(super) struct IoCtx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    _pinned: PhantomPinned,
    _use_p_: PhantomData<P>,
    _use_t_: PhantomData<[T]>,
    buffer_: B,
    ctx_st_: IoCtxState,
    demand_: Option<Demand<O>>,
}

impl<B, P, T, O> IoCtx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    pub const fn new(buffer: B, ctx_st: IoCtxState) -> Self {
        IoCtx {
            _pinned: PhantomPinned,
            _use_p_: PhantomData,
            _use_t_: PhantomData,
            buffer_: buffer,
            ctx_st_: ctx_st,
            demand_: Option::None,
        }
    }

    pub fn buffer(&self) -> &RingBuffer<P, T, O> {
        self.buffer_.borrow()
    }

    pub fn state(&self) -> &IoCtxState {
        &self.ctx_st_
    }

    #[inline]
    pub fn demand_mut(
        self: Pin<&mut Self>,
    ) -> Option<&mut Demand<O>> {
        let this = unsafe { self.get_unchecked_mut() };
        this.demand_.as_mut()
    }

    pub fn try_init_demand(
        self: Pin<&mut Self>,
        demand: Demand<O>,
    ) -> Result<&mut Demand<O>, Demand<O>> {
        let this = unsafe { self.get_unchecked_mut() };
        if this.demand_.is_none() {
            this.demand_ = Option::Some(demand);
            let Option::Some(demand_mut) = &mut this.demand_ else {
                unreachable!()
            };
            Result::Ok(demand_mut)
        } else {
            Result::Err(demand)
        }
    }

    pub fn try_reset_demand(self: Pin<&mut Self>) -> bool {
        let this = unsafe { self.get_unchecked_mut() };
        let opt = this.demand_.take();
        opt.is_some()
    }
}

impl<B, P, T, O> AsMut<B> for IoCtx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    fn as_mut(&mut self) -> &mut B {
        &mut self.buffer_
    }
}

pub(super) struct IoCtxState(AtomicUsize);

impl IoCtxState {
    /// Set the MSB to 1 to flag NO_CLOSE
    const NO_CLOSE_FLAG: usize = 1usize << (usize::BITS - 1);

    pub const fn closing_flag() -> Self {
        Self(AtomicUsize::new(0usize))
    }

    pub const fn no_close_flag() -> Self {
        Self(AtomicUsize::new(Self::NO_CLOSE_FLAG))
    }

    #[inline(always)]
    fn atomic_count_(&self) -> AtomicCount<usize, &mut AtomicUsize> {
        let x = self as *const _ as *mut Self;
        unsafe { AtomicCount::new(&mut (*x).0) }
    }

    #[inline(always)]
    fn get_use_count_(s: usize) -> usize {
        s & (!Self::NO_CLOSE_FLAG)
    }

    /// Returns if the flag indicate the input or output end should close;
    /// true, should close, false, no close.
    #[inline(always)]
    pub fn test_closing_flagged(s: usize) -> bool {
        s | (!Self::NO_CLOSE_FLAG) != usize::MAX
    }

    pub fn incr_use_count(&self) -> CtrlHint {
        let c = self.atomic_count_().inc();
        CtrlHint::NoOp(Self::get_use_count_(c))
    }

    pub fn decr_use_count(&self) -> CtrlHint {
        let s = self.atomic_count_().dec();
        let c = Self::get_use_count_(s);
        if c == 1 && Self::test_closing_flagged(s) {
            CtrlHint::MarkClose(c)
        } else {
            CtrlHint::NoOp(c)
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum CtrlHint {
    MarkClose(usize),
    NoOp(usize),
}

impl fmt::Display for CtrlHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CtrlHint::MarkClose(c) => {
                let c = *c;
                write!(f, "IoCtrl::MarkClose({c})")
            }
            CtrlHint::NoOp(c) => {
                let c = *c;
                write!(f, "IoCtrl::NoOp({c})")
            }
        }
    }
}

pub(super) struct BuffState<B, T = u8, O = StrictOrderings>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    _unuse_t_: PhantomData<NonNull<[MaybeUninit<T>]>>,

    _pinned_: PhantomPinned,

    /// The slot stores the enqueued consumer demand.  
    /// 
    /// Possible pointer values are:
    /// * `ptr::null_mut()`: No enqueued consumer;
    /// * `BuffState::closed_demand_ptr_()`: consumer end closed;
    /// * `BuffState::locked_demand_ptr_()`: consumer demand locked;
    /// * A pointer to an existing `Demand<O>`
    rx_demand_: AtomicDemandPtr<O>,

    /// The slot stores the enqueued producer demand.
    /// 
    /// Possible pointer values are:
    /// * `ptr::null_mut()`: No enqueued producer;
    /// * `BuffState::closed_demand_ptr_()`: producer end closed;
    /// * `BuffState::locked_demand_ptr_()`: producer demand locked;
    /// * A pointer to an existing `Demand<O>`
    tx_demand_: AtomicDemandPtr<O>,

    rw_state_: RwState<O>,

    buf_cell_: UnsafeCell<B>,
}

impl<B, T, O> BuffState<B, T, O>
where
    B: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    pub const fn new_unchecked(buffer: B, capacity: usize) -> Self {
        BuffState {
            _unuse_t_: PhantomData,
            _pinned_: PhantomPinned,
            rx_demand_: AtomicDemandPtr::new(AtomicPtr::new(ptr::null_mut())),
            tx_demand_: AtomicDemandPtr::new(AtomicPtr::new(ptr::null_mut())),
            rw_state_: RwState::new(capacity),
            buf_cell_: UnsafeCell::new(buffer)
        }
    }

    pub fn try_new(buffer: B) -> Result<Self, usize> {
        let s = buffer.borrow().len();
        if s >= RwState::<O>::POS_MAX {
            return Result::Err(s);
        }
        Result::Ok(Self::new_unchecked(buffer, s))
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.rw_state_.capacity()
    }

    #[inline(always)]
    pub fn data_size(&self) -> usize {
        self.rw_state_.data_size()
    }

    #[allow(unused)]
    #[inline]
    pub fn load_state_info(&self) -> RwStateInfo<usize> {
        self.rw_state_.load_state()
    }

    pub fn is_closing(&self) -> bool {
        Self::is_closed_(&self.rx_demand_) || Self::is_closed_(&self.tx_demand_)
    }

    #[inline(always)]
    const fn closed_demand_ptr_() -> *mut Demand<O> {
        usize::MAX as *mut _
    }

    #[inline(always)]
    fn locked_demand_ptr_() -> *mut Demand<O> {
        (usize::MAX - 1) as *mut _
    }

    #[inline(always)]
    fn is_closed_(a: &AtomicDemandPtr<O>) -> bool {
        ptr::eq(a.pointer(), Self::closed_demand_ptr_())
    }

    pub fn try_peek(
        &self,
    ) -> Result<Dual<NonNull<[T]>>, RxError<usize>> {
        let info = self.rw_state_.load_state();
        if info.rl == 0usize {
            let e = if self.is_closing() {
                RxError::Closing
            } else {
                RxError::Drained(info.rp)
            };
            Result::Err(e)
        } else {
            let length = self.capacity();
            Result::Ok(self.pack_slice_read_(&info, length))
        }
    }

    pub fn try_read(
        &self,
        length: usize,
    ) -> Result<Dual<NonNull<[T]>>, RxError<usize>> {
        let info = self.rw_state_.load_state();
        if info.rl == 0usize {
            let e = if self.is_closing() {
                RxError::Closing
            } else {
                RxError::Drained(info.rp)
            };
            Result::Err(e)
        } else {
            Result::Ok(self.pack_slice_read_(&info, length))
        }
    }

    pub fn rx_forward(
        &self,
        length: usize,
    ) -> Result<usize, RxError<usize>> {
        let r = self
            .rx_checked_inc_pos_(length)
            .map(|delta| delta.map_to_usize().amount);
        if r.is_ok() {
            self.try_signal_tx();
        };
        r
    }

    fn rx_checked_inc_pos_(
        &self,
        length: usize,
    ) -> Result<BuffIoDelta<usize>, RxError<usize>> {
        let i = self.rw_state_.load_state();
        // #[cfg(test)]
        // log::trace!("[BuffState::reader_checked_inc_pos_] {i:?} ({:?})", self.rw_state_);
        if i.rl == 0usize {
            let e = if self.is_closing() {
                RxError::Closing
            } else {
                RxError::Drained(i.rp)
            };
            return Result::Err(e);
        };
        self.rw_state_.try_inc_reader_pos(length)
    }

    #[allow(clippy::type_complexity)]
    pub fn try_write(
        &self,
        length: usize,
    ) -> Result<Dual<NonNull<[MaybeUninit<T>]>>, TxError<usize>> {
        let i = self.rw_state_.load_state();
        if i.wl == 0usize {
            let e = if self.is_closing() {
                TxError::Closing
            } else {
                TxError::Stuffed(i.wp)
            };
            Result::Err(e)
        } else {
            Result::Ok(self.pack_slice_write_(&i, length))
        }
    }

    pub fn tx_forward(
        &self,
        length: usize,
    ) -> Result<usize, TxError<usize>> {
        let r = self
            .tx_checked_inc_pos_(length)
            .map(|delta| delta.map_to_usize().amount);
        if r.is_ok() {
            self.try_signal_rx();
        };
        r
    }

    fn tx_checked_inc_pos_(
        &self,
        length: usize,
    ) -> Result<BuffIoDelta<usize>, TxError<usize>> {
        let i = self.rw_state_.load_state();
        // #[cfg(test)]
        // log::trace!("[BuffState::writer_checked_inc_pos_] {i:?} ({:?})", self.rw_state_);
        if i.wl == 0usize {
            let e = if self.is_closing() {
                TxError::Closing
            } else {
                TxError::Stuffed(i.wp)
            };
            return Result::Err(e);
        }
        self.rw_state_.try_inc_writer_pos(length)
    }

    fn pack_slice_write_(
        &self,
        i: &RwStateInfo<usize>,
        length: usize,
    ) -> Dual<NonNull<[MaybeUninit<T>]>> {
        debug_assert!(i.wl > 0usize);
        let mut dual = Dual::new();
        let mut buf_ptr = self.get_buff_non_null_();
        let buf_mut: &mut [MaybeUninit<T>] = unsafe { buf_ptr.as_mut() };

        // Make sure the 1st slice will not exceed the amount needed.
        let l0 = cmp::min(i.wl, length);
        let s0 = &mut buf_mut[i.wp..i.wp + l0];
        dual.push(unsafe { NonNull::new_unchecked(s0) });

        // #[cfg(test)]
        // log::trace!("[BuffState::pack_slice_write_] i({i:?}), l0({l0})");

        // length.saturating_sub(l0) is equivalent to:
        // if l0 < length { length - l0 } else { 0 }
        let l1 = length.saturating_sub(l0);
        if l1 == 0usize || i.wp < i.rp {
            return dual;
        }
        debug_assert!(i.wp >= i.rp);
        debug_assert!(i.wp + l0 == buf_mut.len());
        // Make sure the 2nd slice will not exceed the reader position;
        let l1 = cmp::min(i.rp, l1);
        if l1 > 0 {
            // #[cfg(test)]
            // log::trace!("[BuffState::pack_slice_write_] i({i:?}), l1({l1})");
            let s1 = &mut buf_mut[..l1];
            dual.push(unsafe { NonNull::new_unchecked(s1) });
        }
        dual
    }

    fn pack_slice_read_(
        &self,
        i: &RwStateInfo<usize>,
        length: usize,
    ) -> Dual<NonNull<[T]>> {
        debug_assert!(i.rl > 0usize);
        let mut dual = Dual::new();
        let mut buf_ptr = self.get_buff_non_null_();

        let buf_mut: &mut [T] = unsafe {
            let p = buf_ptr.as_mut() as *mut [MaybeUninit<T>];
            &mut (*(p as *mut [T]))
        };

        // Make sure the 1st slice will not exceed the tail of the buffer.
        let l0 = cmp::min(i.rl, length);
        let s0 = &mut buf_mut[i.rp..i.rp + l0];
        dual.push(unsafe { NonNull::new_unchecked(s0) });

        // #[cfg(test)]
        // log::trace!("[BuffState::pack_slice_read_] i({i:?}), l0({l0})");

        // length.saturating_sub(l0) is equivalent to:
        // if l0 < length { length - l0 } else { 0 }
        let l1 = length.saturating_sub(l0);
        if l1 == 0usize || i.rp < i.wp {
            return dual;
        }
        debug_assert!(i.rp >= i.wp);
        debug_assert!(i.rp + l0 == buf_mut.len());
        // Make sure the 2nd slice will not exceed the writer position
        let l1 = cmp::min(l1, i.wp);
        if l1 > 0 {
            // #[cfg(test)]
            // log::trace!("[BuffState::pack_slice_read_] i({i:?}), l1({l1})");
            let s1 = &mut buf_mut[..l1];
            dual.push(unsafe { NonNull::new_unchecked(s1) });
        }
        dual
    }

    pub fn mark_rx_closed(&self) {
        let x = Self::mark_closed_(&self.rx_demand_);
        assert!(x.is_succ());
        self.try_signal_tx();
    }

    pub fn mark_tx_closed(&self) {
        let x = Self::mark_closed_(&self.tx_demand_);
        assert!(x.is_succ());
        self.try_signal_rx();
    }

    fn mark_closed_(
        cell: &AtomicDemandPtr<O>,
    ) -> CmpxchResult<*mut Demand<O>> {
        let expect = |p: *mut Demand<O>| p.is_null();
        let desire = |_| Self::closed_demand_ptr_();
        cell.try_spin_compare_exchange_weak(expect, desire)
    }

    pub fn enqueue_rx(&self, demand: &Demand<O>) -> bool {
        // #[cfg(test)]
        // log::trace!("[BuffState::enqueue_rx] {demand:p}");
        Self::enqueue_demand_(&self.rx_demand_, demand)
    }

    pub fn enqueue_tx(&self, demand: &Demand<O>) -> bool {
        // #[cfg(test)]
        // log::trace!("[BuffState::enqueue_tx] {:p}", demand);
        Self::enqueue_demand_(&self.tx_demand_, demand)
    }

    fn enqueue_demand_(
        cell: &AtomicDemandPtr<O>,
        demand: &Demand<O>,
    ) -> bool {
        let expect = |p: *mut Demand<O>| p.is_null();
        let desire = |_| demand as *const _ as *mut _;
        cell.try_spin_compare_exchange_weak(expect, desire)
            .is_succ()
    }

    pub fn dequeue_rx(&self, demand: &Demand<O>) -> bool {
        // #[cfg(test)]
        // log::trace!("[BuffState::dequeue_rx] {demand:p}");
        self.dequeue_demand(&self.rx_demand_, demand)
    }

    pub fn dequeue_tx(&self, demand: &Demand<O>) -> bool {
        // #[cfg(test)]
        // log::trace!("[BuffState::dequeue_tx] {demand:p}");
        self.dequeue_demand(&self.tx_demand_, demand)
    }

    fn dequeue_demand(
        &self,
        cell: &AtomicDemandPtr<O>,
        demand: &Demand<O>,
    ) -> bool {
        let p = unsafe {
            NonNull::new_unchecked(demand as *const _ as  *mut _)
        };
        cell.try_spin_compare_and_reset(p)
            .is_ok()
    }

    /// Send signal to the demand stored in the `unsignal_` slot. This may not
    /// succeed if the `chk_fn` in demand denies to signal.
    pub fn try_signal_rx(&self) {
        // #[cfg(test)]
        // log::trace!("[BuffState::try_signal_rx]");
        self.try_signal_(&self.rx_demand_)
    }

    /// Send signal to the demand stored in the `unsignal_` slot. This may not
    /// succeed if the `chk_fn` in demand denies to signal.
    pub fn try_signal_tx(&self) {
        // #[cfg(test)]
        // log::trace!("[BuffState::try_signal_tx]");
        self.try_signal_(&self.tx_demand_)
    }

    fn try_signal_(&self, cell: &AtomicDemandPtr<O>) {
        // Try to lock up the demand by swapping it out of the queue, replacing
        // with locked_demand_ptr_
        let try_lock = {
            let expect = |p: *mut Demand<O>|
                !p.is_null()
                    && !ptr::eq(p, Self::closed_demand_ptr_())
                    && !ptr::eq(p, Self::locked_demand_ptr_());
            let desire = |_|
                Self::locked_demand_ptr_();
            loop {
                let r = cell.try_spin_compare_exchange_weak(expect, desire);
                let CmpxchResult::Unexpected(v) = r else {
                    debug_assert!(matches!(r, CmpxchResult::Succ(_)));
                    break r;
                };
                if ptr::eq(v, Self::locked_demand_ptr_()) {
                    continue;
                } else {
                    // #[cfg(test)]
                    // log::trace!("[BuffState::try_signal_] no demand or closed.");
                    return;
                }
            }
        };
        let CmpxchResult::Succ(p_demand) = try_lock else {
            unreachable!("[BuffState::try_signal_]");
        };
        let demand = unsafe { &mut *p_demand };
        if !demand.check_state(&self.rw_state_) {
            // #[cfg(test)]
            // log::trace!("[BuffState::try_signal_] demand({demand:p}) denied");

            let expect = |p: *mut Demand<O>|
                ptr::eq(p, Self::locked_demand_ptr_());
            let desire = |_| p_demand;
            let r = cell.try_spin_compare_exchange_weak(expect, desire);
            debug_assert!(r.is_succ());
            return;
        };
        let try_send = demand.try_take_waker().map(|w| w.wake());
        assert!(try_send.is_some());

        // #[cfg(test)]
        // log::trace!("[BuffState::try_signal_] signaled demand({demand:p})");

        let try_reset = cell.try_spin_compare_and_reset(unsafe {
            NonNull::new_unchecked(Self::locked_demand_ptr_())
        });
        assert!(try_reset.is_ok());
    }

    fn get_buff_non_null_(&self) -> NonNull<[MaybeUninit<T>]> {
        let as_mut = unsafe { self.buf_cell_.get().as_mut() };
        let Option::Some(b) = as_mut else {
            unreachable!("[BuffState::get_buff_mut_] b")
        };
        let Option::Some(p) = NonNull::new(b.borrow_mut()) else {
            unreachable!("[BuffState::get_buff_mut_] p")
        };
        p
    }

    pub fn buffer_data(&self) -> &[MaybeUninit<T>] {
        unsafe { self.get_buff_non_null_().as_ref() }
    }
}

impl<P, T, O> fmt::Display for BuffState<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    T: Debug,
    O: TrCmpxchOrderings,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let a = unsafe { self.get_buff_non_null_().as_mut() };
        write!(f, "{}|{:?}", &self.rw_state_, a)
    }
}

unsafe impl<P, T, O> Send for BuffState<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    T: Send,
    O: TrCmpxchOrderings,
{}

unsafe impl<P, T, O> Sync for BuffState<P, T, O>
where
    P: BorrowMut<[MaybeUninit<T>]>,
    T: Send + Sync,
    O: TrCmpxchOrderings,
{}

#[derive(Debug)]
pub(super) struct RwStateInfo<U>
where
    U: funty::Unsigned,
{
    /// reader position (offset to the start)
    pub rp: U,

    /// writer position (offset to the start)
    pub wp: U,

    /// Max number of continuous units available for reading from `rp`
    pub rl: U,

    /// Max number of continuous units available for writing from `wp`
    pub wl: U,
}

pub(super) struct RwState<O>
where
    O: TrCmpxchOrderings,
{
    _use_o_: PhantomAtomicPtr<O>,
    rw_pos_: AtomicUsize,
    capacity_: usize,
}

impl<O> RwState<O>
where
    O: TrCmpxchOrderings,
{
    pub const fn new(capacity: usize) -> Self {
        RwState {
            _use_o_: PhantomData,
            rw_pos_: AtomicUsize::new(0usize),
            capacity_: capacity,
        }
    }

    #[inline]
    pub const fn capacity(&self) -> usize {
        self.capacity_
    }

    #[inline]
    pub fn data_size(&self) -> usize {
        self.load_state().rl
    }
}

impl<O> AsRef<AtomicUsize> for RwState<O>
where
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &AtomicUsize {
        &self.rw_pos_
    }
}

impl<O> TrAtomicFlags<usize, O> for RwState<O>
where
    O: TrCmpxchOrderings,
{}

impl<O> RwState<O>
where
    O: TrCmpxchOrderings,
{
    pub const FLAG_RSV_BITS: u32 = 2;

    /// To indicate that writer position overflows the capacity and is not
    /// greater than the reader position.
    const INVERT_FLAG: usize = 1usize << (usize::BITS - 1);

    const MAX_SIZE_BITS: u32 = (usize::BITS - Self::FLAG_RSV_BITS) >> 1;

    const BUFF_SIZE_MOD: usize = 1usize << Self::MAX_SIZE_BITS;

    const WRITER_POS_MASK: usize = Self::BUFF_SIZE_MOD - 1;
    const READER_POS_MASK: usize = Self::WRITER_POS_MASK << Self::MAX_SIZE_BITS;

    const POS_MAX: usize = Self::WRITER_POS_MASK;

    /// Check positions of reader and writer, and calculate the max continuous
    /// buffer size for reading and writing
    fn load_positions_(&self, state: usize) -> RwStateInfo<usize> {
        let wp = Self::load_writer_pos_(state);
        let rp = Self::load_reader_pos_(state);
        let (rl, wl) = if Self::expect_invert_true_(state) {
            debug_assert!(wp <= rp, "wp({wp}) <= rp({rp}), {self:?}");
            (self.capacity_ - rp, rp - wp)
        } else {
            debug_assert!(wp >= rp,  "wp({wp}) >= rp({rp}), {self:?}");
            (wp - rp, self.capacity_ - wp)
        };
        let info = RwStateInfo { rp, wp, rl, wl };
        #[cfg(test)]
        log::trace!("[RwState::load_positions] state({state}), info({info:?})");
        info
    }

    pub fn load_state(&self) -> RwStateInfo<usize> {
        self.load_positions_(self.value())
    }

    /// Try to increase the writer position with `amount`. On success, returns
    /// `Ok` with actual increment which is no greater than `inc`, `Err` with
    /// the state value otherwise.
    fn try_inc_writer_pos(
        &self,
        amount: usize,
    ) -> Result<BuffIoDelta<usize>, TxError<usize>> {
        debug_assert!(amount <= self.capacity_);
        let mut state = self.value();
        loop {
            let rp = Self::load_reader_pos_(state);
            let wp = Self::load_writer_pos_(state);
            // #[cfg(test)]
            // log::trace!(
            //     "[RwState::try_inc_writer_pos] before amount({amount}): \
            //     capacity({}), state({self})", self.capacity_,
            // );
            let s_new;
            let delta;
            if Self::expect_invert_true_(state) {
                debug_assert!(wp <= rp, "wp({wp}) >= rp({rp})");
                let wl = rp - wp;
                if wl == 0usize && amount > 0usize {
                    break Result::Err(TxError::Stuffed(wp));
                }
                delta = cmp::min(wl, amount);
                // When the overflow flag is on, increasing the writer position
                // should never reset the overflow flag.
                s_new = Self::store_writer_pos_(state, wp + delta);
            } else {
                debug_assert!(wp >= rp, "wp({wp}) >= rp({rp})");
                let available = self.capacity_ - wp + rp;
                if available == 0usize && amount > 0usize {
                    break Result::Err(TxError::Stuffed(wp));
                }
                delta = cmp::min(available, amount);
                if delta > 0usize {
                    let w_new = (wp + delta) % self.capacity_;
                    s_new = if w_new < rp || w_new <= wp {
                        let s = Self::store_writer_pos_(state, w_new);
                        Self::desire_invert_true_(s)
                    } else {
                        Self::store_writer_pos_(state, w_new)
                    }
                } else {
                    s_new = state
                }
            }
            let xch_res = self.rw_pos_.compare_exchange_weak(
                state,
                s_new,
                StrictOrderings::SUCC_ORDERING,
                StrictOrderings::FAIL_ORDERING,
            );
            if let Result::Err(x) = xch_res {
                state = x;
                continue;
            }
            // #[cfg(test)]
            // log::trace!(
            //     "[RwState::try_inc_writer_pos] after  amount({amount}): \
            //     capacity({}), state({self})", self.capacity_,
            // );
            break Result::Ok(BuffIoDelta {
                amount: delta,
                offset: wp,
            });
        }
    }

    /// Try to increase the reader position with `amount`. On success, returns
    /// `Ok` with actual increment which is no greater than `amount`, `Err` with
    /// the state value otherwise.
    fn try_inc_reader_pos(
        &self,
        amount: usize,
    ) -> Result<BuffIoDelta<usize>, RxError<usize>> {
        debug_assert!(amount <= self.capacity_);
        let mut state = self.value();
        loop {
            let rp = Self::load_reader_pos_(state);
            let wp = Self::load_writer_pos_(state);
            // #[cfg(test)]
            // log::trace!(
            //     "[RwState::try_inc_reader_pos] before amount({amount}): \
            //     capacity({}), state({self})", self.capacity(),
            // );
            let s_new;
            let delta;
            if Self::expect_invert_true_(state) {
                debug_assert!(rp >= wp, "rp({rp}) >= wp({wp})");
                let available = self.capacity_ - rp + wp;
                if available == 0usize && amount > 0usize {
                    break Result::Err(RxError::Drained(rp));
                }
                delta = cmp::min(available, amount);
                let r_new = (rp + delta) % self.capacity_;
                s_new = if r_new <= wp || r_new <= rp {
                    let s = Self::store_reader_pos_(state, r_new);
                    Self::desire_invert_false_(s)
                } else {
                    Self::store_reader_pos_(state, r_new)
                };
            } else {
                debug_assert!(rp <= wp, "rp({rp}) <= wp({wp})");
                let rl = wp - rp;
                if rl == 0usize && amount > 0usize {
                    break Result::Err(RxError::Drained(rp));
                }
                delta = cmp::min(rl, amount);
                // it is impossible that the increment will reset the overflow flag
                s_new = Self::store_reader_pos_(state, rp + delta);
            }
            let xch_res = self.rw_pos_.compare_exchange_weak(
                state,
                s_new,
                O::SUCC_ORDERING,
                O::FAIL_ORDERING,
            );
            if let Result::Err(x) = xch_res {
                state = x;
                continue;
            }
            // #[cfg(test)]
            // log::trace!(
            //     "[RwState::try_inc_reader_pos] after  amount({amount}): \
            //     capacity({}), state({self})", self.capacity_,
            // );
            break Result::Ok(BuffIoDelta {
                amount: delta,
                offset: rp,
            });
        }
    }

    // -- OVRFLOW_FLAG

    fn expect_invert_true_(value: usize) -> bool {
        value | (!Self::INVERT_FLAG) == usize::MAX
    }

    fn desire_invert_false_(value: usize) -> usize {
        value & (!Self::INVERT_FLAG)
    }

    fn desire_invert_true_(value: usize) -> usize {
        value | Self::INVERT_FLAG
    }

    // --

    fn load_writer_pos_(value: usize) -> usize {
        value & Self::WRITER_POS_MASK
    }

    fn store_writer_pos_(value: usize, pos: usize) -> usize {
        value & (!Self::WRITER_POS_MASK) | pos
    }

    fn load_reader_pos_(value: usize) -> usize {
        (value & Self::READER_POS_MASK) >> Self::MAX_SIZE_BITS
    }

    fn store_reader_pos_(value: usize, pos: usize) -> usize {
        (value & (!Self::READER_POS_MASK)) | (pos << Self::MAX_SIZE_BITS)
    }
}

impl<O> fmt::Debug for RwState<O>
where
    O: TrCmpxchOrderings,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl<O> fmt::Display for RwState<O>
where
    O: TrCmpxchOrderings,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.value();
        let r = Self::load_reader_pos_(s);
        let w = Self::load_writer_pos_(s);
        write!(f, "r: {r}, ")?;
        write!(f, "w: {w}, ")?;
        if Self::expect_invert_true_(s) {
            write!(f, "INVERT")
        } else {
            write!(f, "NORMAL")
        }
    }
}

#[derive(Debug)]
pub(super) struct BuffIoDelta<U: funty::Unsigned> {
    /// The amount of increment
    pub amount: U,

    /// The index where increment starts
    pub offset: U,
}

impl<U: funty::Unsigned> BuffIoDelta<U>  {
    pub fn map<X: funty::Unsigned>(
        self,
        map: impl Fn(&U) -> X,
    ) -> BuffIoDelta<X> {
        let amount = map(&self.amount);
        let offset = map(&self.offset);
        BuffIoDelta { amount, offset }
    }

    pub fn map_to_usize(self) -> BuffIoDelta<usize> {
        const HINT: &str = "[BuffIoDelta::map_to_usize]";
        self.map(|u| d_to_usize(*u, HINT))
    }
}

#[inline(always)]
pub(super) fn d_to_usize<D: funty::Unsigned>(d: D,  m: &'static str) -> usize {
    let Result::Ok(u) = d.try_into() else {
        unreachable!("{m}")
    };
    u
}

#[cfg(test)]
mod tests_ {
    use std::{
        borrow::*,
        boxed::Box,
        mem::MaybeUninit,
        sync::Arc,
    };
    use atomex::{
        x_deps::funty,
        StrictOrderings, TrAtomicFlags, TrCmpxchOrderings,
    };
    use core_malloc::CoreAlloc;
    use mm_ptr::Owned;
    use spmv_oneshot::x_deps::atomex;
    use crate::ring_buffer::{RxError, TxError};

    use super::{BuffState, RwState};

    #[test]
    fn rw_state_smoke() {
        const BUFF_SIZE: usize = 16usize;
        let rw = RwState::<StrictOrderings>::new(BUFF_SIZE);
        let s = rw.value();
        assert_eq!(s, 0);
        assert_eq!(rw.capacity(), BUFF_SIZE);
        assert_eq!(rw.data_size(), 0usize);

        assert!(!RwState::<StrictOrderings>::expect_invert_true_(s));
        let info = rw.load_positions_(s);
        assert_eq!(info.rp, 0);
        assert_eq!(info.wp, 0);
        assert_eq!(info.rl, 0);
        assert_eq!(info.wl, BUFF_SIZE);
    }

    #[test]
    fn read_wrote_overflow_smoke() {
        let _ = env_logger::builder().is_test(true).try_init();

        const BUFF_SIZE: usize = 4usize;
        let Result::Ok(buff) =
            BuffState::<Box<[MaybeUninit<u8>]>>::try_new(
                Box::<[u8]>::new_uninit_slice(BUFF_SIZE))
        else {
            panic!()
        };
        let mut c = 0usize;
        // 1. Fill the ring buffer
        loop {
            if c >= BUFF_SIZE { break; }
            match buff.try_write(BUFF_SIZE - c) {
                Result::Ok(dual) => {
                    for mut p in dual.into_iter() {
                        let target = unsafe { p.as_mut() };
                        let len = target.len();
                        assert!(len <= BUFF_SIZE - c);
                        c += len;
                        let x = buff.tx_forward(len);
                        assert!(x.is_ok());
                    }
                    continue;
                },
                Result::Err(TxError::Stuffed(_)) => continue,
                Result::Err(e) => panic!("{e:?}"),
            }
        };
        // 2. Read some bytes from the front
        let mut c = 0usize;
        loop {
            if c >= BUFF_SIZE - 1 { break; }
            match buff.try_read(BUFF_SIZE - c) {
                Result::Ok(dual) => {
                    for p in dual.into_iter() {
                        let src = unsafe { p.as_ref() };
                        let len = src.len();
                        assert!(len <= BUFF_SIZE - c);
                        // we don't actually read the content, just drop it
                        c += len;
                        let x = buff.rx_forward(len);
                        assert!(x.is_ok());
                    }
                    continue;
                },
                Result::Err(RxError::Drained(_)) => continue,
                Result::Err(e) => panic!("{e:?}"),
            }
        };
        // 3. Write some byte into the front
        assert!(buff.try_write(BUFF_SIZE - 2).is_ok());
        assert!(buff.tx_forward(BUFF_SIZE - 2).is_ok());
        let try_read = buff.try_read(BUFF_SIZE);
        assert!(try_read.is_ok());
        let buf = try_read.ok().unwrap();
        assert!(buff.rx_forward(buf.len()).is_ok());
    }

    /// Write [0][0..1][0..2]..[0..max_len - 1]
    fn writer_<P, T, O>(s: Arc<BuffState<P, T, O>>, max_len: usize)
    where
        P: BorrowMut<[MaybeUninit<T>]>,
        T: funty::Unsigned + TryFrom<usize> + Copy,
        O: TrCmpxchOrderings,
    {
        let mut seq_len = 1usize;
        loop {
            if seq_len > max_len {
                break;
            }
            // generate [0..seq_len - 1]
            let source = Owned::new_slice(
                seq_len,
                |u, m| {
                    let Result::Ok(x) = T::try_from(u) else { panic!() };
                    m.write(x);
                },
                CoreAlloc::new(),
            );
            let mut wrote_len = 0usize;
            // write all in [0..seq_len] into buffer
            loop {
                let split = source.split_at(wrote_len);
                let src = split.1;
                match s.try_write(src.len()) {
                    Result::Ok(dual) => {
                        let mut wc = 0usize;
                        for mut p in dual.into_iter() {
                            let dst = unsafe { p.as_mut() };
                            assert!(wc + dst.len() <= src.len());
                            let dst = unsafe {
                                &mut *(dst as *mut _ as *mut [T])
                            };
                            dst.clone_from_slice(&src[wc..wc + dst.len()]);
                            wc += dst.len();
                            let x = s.tx_forward(dst.len());
                            assert!(x.is_ok());
                        }
                        wrote_len += wc;
                        if wrote_len == source.len() {
                            // log::trace!("writer #{seq_len}: {:?} ({})", source.as_ref(), *s);
                            break;
                        }
                    },
                    Result::Err(TxError::Stuffed(_)) => continue,
                    Result::Err(_) => break,
                }
            }
            seq_len += 1;
        }
        s.mark_tx_closed();
        log::trace!("writer exits")
    }

    /// Read [0][0..1]..[0..max_len - 1] with size-decreasing buffers
    fn reader_<P, T, O>(s: Arc<BuffState<P, T, O>>, max_len: usize)
    where
        P: BorrowMut<[MaybeUninit<T>]>,
        T: funty::Unsigned + TryInto<usize> + Copy,
        O: TrCmpxchOrderings,
    {
        let mut seq_len = 1usize;
        let mut auth_length = 1usize;
        let mut auth_offset = 0usize;
        loop {
            if seq_len > max_len - 1 {
                break;
            }
            // generate [0..seq_len - 1]
            let mut target = Owned::new_slice(
                max_len - seq_len,
                |_, m| { m.write(T::ZERO); },
                CoreAlloc::new(),
            );
            // how many units has been copied to target
            let mut read_offset = 0usize;
            // read [0..seq_len - 1] from the buffer
            loop {
                let split = target.split_at_mut(read_offset);
                let dst = split.1;
                match s.try_read(dst.len()) {
                    Result::Ok(dual) => {
                        // how many units has been copied to dst
                        let mut rc = 0usize;
                        for p in dual.into_iter() {
                            let src = unsafe { p.as_ref() };
                            assert!(rc + src.len() <= dst.len());
                            let tgt = &mut dst[rc..rc + src.len()];
                            tgt.clone_from_slice(src);
                            rc += src.len();
                            let x = s.rx_forward(src.len());
                            assert!(x.is_ok());
                        }
                        read_offset += rc;
                        if read_offset == target.len() {
                            // log::trace!("reader #{seq_len}: {:?}", target.as_ref());
                            break;
                        }
                    },
                    Result::Err(RxError::Drained(_)) => continue,
                    Result::Err(_) => break,
                }
            }
            for v in target.iter() {
                let Result::Ok(a) = TryInto::<usize>::try_into(*v) else {
                    panic!()
                };
                assert_eq!(
                    a, auth_offset,
                    "a({a}), auth_offset({auth_offset} / {auth_length})",
                );
                if auth_offset < auth_length - 1 {
                    auth_offset += 1;
                } else if auth_length < max_len {
                    auth_offset = 0;
                    auth_length += 1;
                }
            }
            seq_len += 1;
        }
        s.mark_rx_closed();
        log::trace!("reader exits")
    }

    const TEST_MAX_LEN: usize = 256;

    #[test]
    fn u8_read_write_concurrent_smoke() {
        const BUFF_SIZE: u8 = u8::MAX;
        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(state) =
            BuffState::<Owned<[MaybeUninit<u8>], CoreAlloc>>::try_new(
                Owned::new_zeroed_slice(
                    usize::from(BUFF_SIZE),
                    CoreAlloc::new(),
            ))
        else {
            panic!()
        };
        let s = Arc::new(state);
        let writer_handle = {
            let s_cloned = s.clone();
            std::thread::spawn(move || writer_(s_cloned, TEST_MAX_LEN))
        };
        let reader_handle = std::thread::spawn(move || reader_(s, TEST_MAX_LEN));
        let w = writer_handle.join();
        let r = reader_handle.join();
        assert!(w.is_ok());
        assert!(r.is_ok());
    }

    #[test]
    fn u16_read_write_concurrent_smoke() {
        const BUFF_SIZE: u16 = 1024u16;
        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(state) =
            BuffState::<Owned<[MaybeUninit<u16>], CoreAlloc>, u16>::try_new(
                Owned::new_zeroed_slice(
                    usize::from(BUFF_SIZE), 
                    CoreAlloc::new(),
            ))
        else {
            panic!()
        };
        let s = Arc::new(state);
        let s_cloned = s.clone();
        let writer_handle = std::thread::spawn(move || writer_(s_cloned, TEST_MAX_LEN));
        let reader_handle = std::thread::spawn(move || reader_(s, TEST_MAX_LEN));
        let w = writer_handle.join();
        let r = reader_handle.join();
        assert!(w.is_ok());
        assert!(r.is_ok());
    }

    #[test]
    fn u32_read_write_concurrent_smoke() {
        const BUFF_SIZE: u32 = 1024u32;
        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(state) =
            BuffState::<Owned<[MaybeUninit<u32>], CoreAlloc>, u32>::try_new(
                Owned::new_zeroed_slice(
                    usize::try_from(BUFF_SIZE).unwrap(),
                    CoreAlloc::new(),
            ))
        else {
            panic!()
        };
        let s = Arc::new(state);
        let s_cloned = s.clone();
        let writer_handle = std::thread::spawn(move || writer_(s_cloned, TEST_MAX_LEN));
        let reader_handle = std::thread::spawn(move || reader_(s, TEST_MAX_LEN));
        let w = writer_handle.join();
        let r = reader_handle.join();
        assert!(w.is_ok());
        assert!(r.is_ok());
    }
}
