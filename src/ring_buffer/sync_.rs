use core::{
    borrow::BorrowMut,
    cell::UnsafeCell,
    cmp,
    fmt::{self, Debug},
    marker::{PhantomData, PhantomPinned},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize},
};

use atomex::{
    x_deps::funty,
    AtomexPtrOwned, CmpxchResult, PhantomAtomicPtr, StrictOrderings,
    TrAtomicFlags, TrCmpxchOrderings,
};
use asyncex::{
    channel::oneshot::Oneshot,
    x_deps::atomex,
};

use super::{RxError, TxError, Dual};

pub(super) type FnCheckState<O> = dyn FnMut(&RwState<O>) -> bool;
pub(super) type AtomicDemandPtr<O> = AtomexPtrOwned<Demand<O>, O>;

#[derive(Debug)]
pub(super) struct CheckStateFn<O>(NonNull<FnCheckState<O>>)
where
    O: TrCmpxchOrderings;

impl<O> CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{
    pub fn new(f: &mut FnCheckState<O>) -> Self {
        let p = unsafe { NonNull::new_unchecked(f) };
        CheckStateFn(p)
    }
}

impl<O> FnOnce<(&RwState<O>, )> for CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{
    type Output = bool;

    extern "rust-call" fn call_once(mut self, args: (&RwState<O>,)) -> Self::Output {
        let f = unsafe { self.0.as_mut() };
        f(args.0)
    }
}

impl<O> FnMut<(&RwState<O>,)> for CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{
    extern "rust-call" fn call_mut(&mut self, args: (&RwState<O>,)) -> Self::Output {
        let f = unsafe { self.0.as_mut() };
        f(args.0)
    }
}

impl<O> Deref for CheckStateFn<O>
where
    O: TrCmpxchOrderings,
{
    type Target = FnCheckState<O>;

    fn deref(&self) -> &Self::Target {
        unsafe { self.0.as_ref() }
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
    pub chk_fn: CheckStateFn<O>,
    pub signal: Oneshot<(), O>,
}

impl<O> Demand<O>
where
    O: TrCmpxchOrderings,
{
    pub fn new(check: &mut FnCheckState<O>) -> Self {
        Demand {
            chk_fn: CheckStateFn::new(check),
            signal: Oneshot::new(),
        }
    }

    pub fn producer_check(state: &RwState<O>) -> bool {
        let i = state.load_state();
        i.writer_length > 0usize
    }

    pub fn consumer_check(state: &RwState<O>) -> bool {
        let i = state.load_state();
        i.reader_length > 0usize
    }
}

pub(super) struct BuffState<B, T = u8, O = StrictOrderings>
where
    B: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    _unuse_t_: PhantomData<[T]>,

    _pinned_: PhantomPinned,

    /// The slot stores the checked but not signaled demand.
    unsignal_: AtomicDemandPtr<O>,

    /// The slot stores the enqueued consumer demand.
    consumer_: AtomicDemandPtr<O>,

    /// The slot stores the enqueued producer demand.
    producer_: AtomicDemandPtr<O>,

    rw_state_: RwState<O>,

    buf_cell_: UnsafeCell<B>,
}

impl<B, T, O> BuffState<B, T, O>
where
    B: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub fn try_new(buffer: B) -> Result<Self, usize> {
        let s = buffer.borrow().len();
        if s >= RwState::<O>::POS_MAX {
            return Result::Err(s);
        }
        Result::Ok(BuffState {
            _unuse_t_: PhantomData,
            _pinned_: PhantomPinned,
            unsignal_: AtomicDemandPtr::new(AtomicPtr::new(ptr::null_mut())),
            consumer_: AtomicDemandPtr::new(AtomicPtr::new(ptr::null_mut())),
            producer_: AtomicDemandPtr::new(AtomicPtr::new(ptr::null_mut())),
            rw_state_: RwState::new(buffer.borrow().len()),
            buf_cell_: UnsafeCell::new(buffer)
        })
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.rw_state_.capacity()
    }

    #[inline(always)]
    pub fn data_size(&self) -> usize {
        self.rw_state_.data_size()
    }

    pub fn is_closing(&self) -> bool {
        Self::is_closed_(&self.consumer_) || Self::is_closed_(&self.producer_)
    }

    #[inline(always)]
    const fn closed_demand_ptr_() -> *mut Demand<O> {
        usize::MAX as *mut _
    }

    #[inline(always)]
    fn is_closed_(a: &AtomicDemandPtr<O>) -> bool {
        ptr::eq(a.pointer(), Self::closed_demand_ptr_())
    }

    pub fn try_peek(
        &self,
    ) -> Result<Dual<NonNull<[T]>>, RxError<usize>> {
        let state = self.rw_state_.load_state();
        if state.reader_length == 0usize {
            let e = if self.is_closing() {
                RxError::Closing
            } else {
                RxError::Drained(state.reader_offset)
            };
            Result::Err(e)
        } else {
            let length = self.capacity();
            Result::Ok(self.pack_slice_read_(&state, length))
        }
    }

    pub fn try_read(
        &self,
        length: usize,
    ) -> Result<Dual<NonNull<[T]>>, RxError<usize>> {
        let state = self.rw_state_.load_state();
        if state.reader_length == 0usize {
            let e = if self.is_closing() {
                RxError::Closing
            } else {
                RxError::Drained(state.reader_offset)
            };
            Result::Err(e)
        } else {
            Result::Ok(self.pack_slice_read_(&state, length))
        }
    }

    pub fn reader_forward(
        &self,
        length: usize,
    ) -> Result<usize, RxError<usize>> {
        let r = self
            .reader_checked_inc_pos_(length)
            .map(|delta| delta.map_to_usize().amount);
        if r.is_ok() {
            self.try_signal_producer();
        };
        r
    }

    fn reader_checked_inc_pos_(
        &self,
        length: usize,
    ) -> Result<BuffIoDelta<usize>, RxError<usize>> {
        let info = self.rw_state_.load_state();
        if info.reader_length == 0usize {
            let e = if self.is_closing() {
                RxError::Closing
            } else {
                RxError::Drained(info.reader_offset)
            };
            return Result::Err(e);
        };
        self.rw_state_.try_inc_reader_pos(length)
    }

    pub fn try_write(
        &self,
        length: usize,
    ) -> Result<Dual<NonNull<[T]>>, TxError<usize>> {
        let s = self.rw_state_.load_state();
        if s.writer_length == 0usize {
            let e = if self.is_closing() {
                TxError::Closing
            } else {
                TxError::Stuffed(s.writer_offset)
            };
            Result::Err(e)
        } else {
            Result::Ok(self.pack_slice_write_(&s, length))
        }
    }

    pub fn writer_forward(
        &self,
        length: usize,
    ) -> Result<usize, TxError<usize>> {
        let r = self
            .writer_checked_inc_pos_(length)
            .map(|delta| delta.map_to_usize().amount);
        if r.is_ok() {
            self.try_signal_consumer();
        };
        r
    }

    fn writer_checked_inc_pos_(
        &self,
        length: usize,
    ) -> Result<BuffIoDelta<usize>, TxError<usize>> {
        let state = self.rw_state_.load_state();
        if state.writer_length == 0usize {
            let e = if self.is_closing() {
                TxError::Closing
            } else {
                TxError::Stuffed(state.writer_offset)
            };
            return Result::Err(e);
        }
        let src_len = if length > state.writer_length {
            state.writer_length
        } else {
            length
        };
        self.rw_state_.try_inc_writer_pos(src_len)
    }

    fn pack_slice_write_(
        &self,
        state: &RwStateInfo<usize>,
        length: usize,
    ) -> Dual<NonNull<[T]>> {
        todo!()
    }

    fn pack_slice_read_(
        &self,
        state: &RwStateInfo<usize>,
        length: usize,
    ) -> Dual<NonNull<[T]>> {
        todo!()
    }

    #[inline(always)]
    pub fn mark_consumer_closed(&self) {
        let x = Self::mark_closed_(&self.consumer_);
        assert!(x.is_succ());
        self.try_signal_producer();
    }

    #[inline(always)]
    pub fn mark_producer_closed(&self) {
        let x = Self::mark_closed_(&self.producer_);
        assert!(x.is_succ());
        self.try_signal_consumer();
    }

    fn mark_closed_(
        cell: &AtomicDemandPtr<O>,
    ) -> CmpxchResult<*mut Demand<O>> {
        let expect = |p: *mut Demand<O>| p.is_null();
        let desire = |_| Self::closed_demand_ptr_();
        cell.try_spin_compare_exchange_weak(expect, desire)
    }

    #[inline(always)]
    pub fn enqueue_consumer(&self, demand: &Demand<O>) -> bool {
        #[cfg(test)]
        log::trace!("[BuffState::enqueue_consumer] {:p}", demand);
        Self::enqueue_demand_(&self.consumer_, demand)
    }

    #[inline(always)]
    pub fn enqueue_producer(&self, demand: &Demand<O>) -> bool {
        #[cfg(test)]
        log::trace!("[BuffState::enqueue_producer] {:p}", demand);
        Self::enqueue_demand_(&self.producer_, demand)
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

    /// Tries to invoke `chk_fn` of demand in enqueued consumer cell, and move
    /// the demand to the `unsignal_` slot if it will not activate at this time.
    #[inline(always)]
    pub fn check_consumer(&self, demand: &Demand<O>) -> bool {
        #[cfg(test)]
        log::trace!("[BuffState::check_consumer] {:p}", demand);
        self.check_demand_(&self.consumer_, demand)
    }

    /// Tries to invoke `chk_fn` of demand in enqueued producer cell, and move
    /// the demand to the `unsignal_` slot if it will not activate at this time.
    #[inline(always)]
    pub fn check_producer(&self, demand: &Demand<O>) -> bool {
        #[cfg(test)]
        log::trace!("[BuffState::check_producer] {:p}", demand);
        self.check_demand_(&self.producer_, demand)
    }

    fn check_demand_(
        &self,
        cell: &AtomicDemandPtr<O>,
        demand: &Demand<O>,
    ) -> bool {
        let expect = |p: *mut Demand<O>| ptr::eq(p, demand);
        let desire = |_| ptr::null_mut();
        let r: Result<_, _> = cell
            .try_spin_compare_exchange_weak(expect, desire)
            .into();
        let Result::Ok(p) = r else {
            #[cfg(test)]
            log::trace!("[BuffState::check_demand_] cannot check({r:?})");
            return false;
        };
        let opt_demand = unsafe { p.as_mut() };
        let Option::Some(demand) = opt_demand else {
            unreachable!()
        };
        let check_fn = &mut demand.chk_fn;
        if !check_fn(&self.rw_state_) {
            #[cfg(test)]
            log::trace!("[BuffState::check_demand_] demand denied");
            let init = unsafe { NonNull::new_unchecked(demand as *mut _) };
            let x = self.unsignal_.try_spin_init(init);
            assert!(x.is_ok());
            return true;
        };
        let x = demand.signal.send(()).wait();

        #[allow(unused_variables)]
        if let Result::Err(e) = &x {
            #[cfg(test)]
            log::warn!("[BuffState::check_demand_] signaling failed({e:?}");
        }
        x.is_ok()
    }

    /// Dequeue a (previously enqueued) consumer demand if not activated.
    /// Return if the demand is successfully dequeued.
    #[inline(always)]
    pub fn dequeue_consumer(&self, demand: &Demand<O>) -> bool {
        #[cfg(test)]
        log::trace!("[BuffState::dequeue_consumer] {:p}", demand);
        Self::dequeue_demand_(&self.consumer_, demand)
    }

    /// Dequeue a (previously enqueued) producer demand if not activated.
    /// Return if the demand is successfully dequeued.
    #[inline(always)]
    pub fn dequeue_producer(&self, demand: &Demand<O>) -> bool {
        #[cfg(test)]
        log::trace!("[BuffState::dequeue_producer] {:p}", demand);
        Self::dequeue_demand_(&self.producer_, demand)
    }

    fn dequeue_demand_(
        cell: &AtomicDemandPtr<O>,
        demand: &Demand<O>,
    ) -> bool {
        let p = unsafe {
            NonNull::new_unchecked(demand as *const _ as * mut _)
        };
        cell.try_spin_compare_and_reset(p)
            .is_ok()
    }

    /// Send signal to the demand stored in the `unsignal_` slot. This may not
    /// succeed if the `chk_fn` in demand denies to signal.
    pub fn try_signal_consumer(&self) {
        #[cfg(test)]
        log::trace!("[BuffState::try_signal_consumer]");
        self.try_signal_()
    }

    /// Send signal to the demand stored in the `unsignal_` slot. This may not
    /// succeed if the `chk_fn` in demand denies to signal.
    pub fn try_signal_producer(&self) {
        #[cfg(test)]
        log::trace!("[BuffState::try_signal_producer]");
        self.try_signal_()
    }

    fn try_signal_(&self) {
        let p = self.unsignal_.pointer();
        if p == Self::closed_demand_ptr_() || p.is_null() {
            #[cfg(test)]
            log::trace!("[BuffState::try_signal_] not demand({p:p})");
            return;
        };
        let opt_demand = unsafe { p.as_mut() };
        let Option::Some(demand) = opt_demand else {
            // we should have avoided null_ptr when cmpxch
            unreachable!("[BuffState::try_signal_] unexpected null demand");
        };
        let check_fn = &mut demand.chk_fn;
        if !check_fn(&self.rw_state_) {
            #[cfg(test)]
            log::trace!("[BuffState::try_signal_] demand denied");
            return;
        };
        let r = self
            .unsignal_
            .try_spin_compare_and_reset(unsafe { NonNull::new_unchecked(p) });
        if r.is_err() {
            #[cfg(test)]
            log::trace!("[BuffState::try_signal_] cmpxch failed.");
            return;
        }
        let x = demand.signal.send(()).wait();
        #[allow(unused_variables)]
        if let Result::Err(e) = x {
            #[cfg(test)]
            log::warn!("[BuffState::try_signal_] signal err({e:?}");
        }
    }

    pub fn abort_consumer(&self, demand: &Demand<O>) -> bool {
        #[cfg(test)]
        log::trace!("[BuffState::abort_consumer]");
        self.abort_demand(demand)
    }

    pub fn abort_producer(&self, demand: &Demand<O>) -> bool {
        #[cfg(test)]
        log::trace!("[BuffState::abort_producer]");
        self.abort_demand(demand)
    }

    fn abort_demand(&self, demand: &Demand<O>) -> bool {
        let p = unsafe {
            NonNull::new_unchecked(demand as *const _ as  *mut _)
        };
        self.unsignal_
            .try_spin_compare_and_reset(p)
            .is_ok()
    }

    fn get_buff_mut_(&self) -> NonNull<[T]> {
        let as_mut = unsafe { self.buf_cell_.get().as_mut() };
        let Option::Some(b) = as_mut else {
            unreachable!("[BuffState::get_buff_mut_] b")
        };
        let Option::Some(p) = NonNull::new(b.borrow_mut()) else {
            unreachable!("[BuffState::get_buff_mut_] p")
        };
        p
    }

    pub fn buffer_data(&self) -> &[T] {
        unsafe { self.get_buff_mut_().as_ref() }
    }
}

impl<P, T, O> fmt::Display for BuffState<P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone + Debug,
    O: TrCmpxchOrderings,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let a = unsafe { self.get_buff_mut_().as_mut() };
        let g = RwPosIoGuard::acquire(self, a);
        write!(f, "{}|{:?}", &self.rw_state_, g.deref())
    }
}

unsafe impl<P, T, O> Send for BuffState<P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone + Send,
    O: TrCmpxchOrderings,
{}

unsafe impl<P, T, O> Sync for BuffState<P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone + Send + Sync,
    O: TrCmpxchOrderings,
{}

pub(super) struct RwStateInfo<U>
where
    U: funty::Unsigned,
{
    pub reader_offset: U,
    pub writer_offset: U,

    /// Number of units available for reading
    pub reader_length: U,

    /// Number of units available for writing
    pub writer_length: U,
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
    pub fn capacity(&self) -> usize {
        self.capacity_
    }

    #[inline]
    pub fn data_size(&self) -> usize {
        self.load_state().reader_length
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

    /// To indicate whether the buffer is locked for operation.
    #[allow(non_snake_case)]
    const IO_BUSY_FLAG: usize = Self::INVERT_FLAG >> 1;

    const MAX_SIZE_BITS: u32 = (usize::BITS - Self::FLAG_RSV_BITS) >> 1;

    const BUFF_SIZE_MOD: usize = 1usize << Self::MAX_SIZE_BITS;

    const WRITER_POS_MASK: usize = Self::BUFF_SIZE_MOD - 1;
    const READER_POS_MASK: usize = Self::WRITER_POS_MASK << Self::MAX_SIZE_BITS;

    const POS_MAX: usize = Self::WRITER_POS_MASK;

    /// Check positions of reader and writer, and calculate the max continuous
    /// buffer size for reading and writing
    fn load_positions_(&self, state: usize) -> RwStateInfo<usize> {
        let w = Self::load_writer_pos_(state);
        let r = Self::load_reader_pos_(state);
        let (l, a) = if Self::expect_invert_true_(state) {
            debug_assert!(w <= r, "w({w}) <= r({r}), {self:?}");
            (self.capacity_ - r, r - w)
        } else {
            debug_assert!(w >= r,  "w({w}) >= r({r}), {self:?}");
            (w - r, self.capacity_ - w)
        };
        RwStateInfo {
            reader_offset: r,
            writer_offset: w,
            reader_length: l,
            writer_length: a,
        }
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
        let mut state = self.value();
        let amount = amount % (self.capacity_ + 1usize);
        loop {
            let r = Self::load_reader_pos_(state);
            let w = Self::load_writer_pos_(state);
            #[cfg(test)]
            log::trace!(
                "[RwState::try_inc_writer_pos] before amount({amount}): \
                capacity({}), state({self})", self.capacity(),
            );
            let s_new;
            let delta;
            if Self::expect_invert_true_(state) {
                debug_assert!(w <= r, "w({w}) >= r({r})");
                let available = r - w;
                if available == 0usize && amount > 0usize {
                    break Result::Err(TxError::Stuffed(w));
                }
                delta = cmp::min(available, amount);
                // When the overflow flag is on, increasing the writer position
                // should never reset the overflow flag.
                s_new = Self::store_writer_pos_(state, w + delta);
            } else {
                debug_assert!(w >= r, "w({w}) >= r({r})");
                let available = self.capacity_ - w + r;
                if available == 0usize && amount > 0usize {
                    break Result::Err(TxError::Stuffed(w));
                }
                delta = cmp::min(available, amount);
                if delta > 0usize {
                    let w_new = (w + delta) % self.capacity_;
                    s_new = if w_new < r || w_new <= w {
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
            #[cfg(test)]
            log::trace!(
                "[RwState::try_inc_writer_pos] after  amount({amount}): \
                capacity({}), state({self})", self.capacity(),
            );
            break Result::Ok(BuffIoDelta {
                amount: delta,
                offset: w,
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
        let mut state = self.value();
        loop {
            let r = Self::load_reader_pos_(state);
            let w = Self::load_writer_pos_(state);
            #[cfg(test)]
            log::trace!(
                "[RwState::try_inc_reader_pos] before amount({amount}): \
                capacity({}), state({self})", self.capacity(),
            );
            let s_new;
            let delta;
            if Self::expect_invert_true_(state) {
                debug_assert!(r >= w, "r({r}) >= w({w})");
                let available = self.capacity_ - r + w;
                if available == 0usize && amount > 0usize {
                    break Result::Err(RxError::Drained(r));
                }
                delta = cmp::min(available, amount);
                let r_new = (r + delta) % self.capacity_;
                s_new = if r_new <= w || r_new <= r {
                    let s = Self::store_reader_pos_(state, r_new);
                    Self::desire_invert_false_(s)
                } else {
                    Self::store_reader_pos_(state, r_new)
                };
            } else {
                debug_assert!(r <= w, "r({r}) <= w({w})");
                let available = w - r;
                if available == 0usize && amount > 0usize {
                    break Result::Err(RxError::Drained(r));
                }
                delta = cmp::min(available, amount);
                // it is impossible that the increment will reset the overflow flag
                s_new = Self::store_reader_pos_(state, r + delta);
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
            #[cfg(test)]
            log::trace!(
                "[RwState::try_inc_reader_pos] after  amount({amount}): \
                capacity({}), state({self})", self.capacity(),
            );
            break Result::Ok(BuffIoDelta {
                amount: delta,
                offset: r,
            });
        }
    }

    pub fn is_io_busy(&self) -> bool {
        Self::expect_io_busy_true_(self.value())
    }

    pub fn try_set_io_busy(&self, is_busy: bool) -> CmpxchResult<usize> {
        if is_busy {
            self.try_spin_compare_exchange_weak(
                Self::expect_io_busy_false_,
                Self::desire_io_busy_true_,
            )
        } else {
            self.try_spin_compare_exchange_weak(
                Self::expect_io_busy_true_,
                Self::desire_io_busy_false_,
            )
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

    // -- IO_BUSY_FLAG

    fn expect_io_busy_true_(value: usize) -> bool {
        value | (!Self::IO_BUSY_FLAG) == usize::MAX
    }

    fn expect_io_busy_false_(value: usize) -> bool {
        value & Self::IO_BUSY_FLAG == 0usize
    }

    fn desire_io_busy_true_(value: usize) -> usize {
        value | Self::IO_BUSY_FLAG
    }

    fn desire_io_busy_false_(value: usize) -> usize {
        value & (!Self::IO_BUSY_FLAG)
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
            write!(f, "INVERT, ")?;
        } else {
            write!(f, "NORMAL, ")?;
        }
        if Self::expect_io_busy_true_(s) {
            write!(f, "BUSY")
        } else {
            write!(f, "FREE")
        }
    }
}

pub(super) struct RwPosIoGuard<'a, P, T, O>(
    &'a BuffState<P, T, O>,
    &'a mut [T])
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<'a, P, T, O> RwPosIoGuard<'a, P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn acquire(
        state: &'a BuffState<P, T, O>,
        buffer: &'a mut [T],
    ) -> RwPosIoGuard<'a, P, T, O> {
        loop {
            let x = state.rw_state_.try_set_io_busy(true);
            if x.is_succ() {
                break;
            }
        }
        assert!(state.rw_state_.is_io_busy());
        RwPosIoGuard(state, buffer)
    }

    fn release_(state: &BuffState<P, T, O>) {
        loop {
            let x = state.rw_state_.try_set_io_busy(false);
            if x.is_succ() {
                break;
            }
        }
    }

    pub fn state(&self) -> &BuffState<P, T, O> {
        self.0
    }
}

impl<P, T, O> Drop for RwPosIoGuard<'_, P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        Self::release_(self.state())
    }
}

impl<P, T, O> Deref for RwPosIoGuard<'_, P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.1
    }
}

impl<P, T, O> DerefMut for RwPosIoGuard<'_, P, T, O>
where
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.1
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
        sync::Arc,
    };
    use atomex::{
        x_deps::funty,
        StrictOrderings, TrAtomicFlags, TrCmpxchOrderings,
    };
    use core_malloc::CoreAlloc;
    use mm_ptr::Owned;
    use asyncex::x_deps::{mm_ptr, atomex};
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
        assert!(!rw.is_io_busy());
        assert!(!RwState::<StrictOrderings>::expect_invert_true_(s));
        let info = rw.load_positions_(s);
        assert_eq!(info.reader_offset, 0);
        assert_eq!(info.writer_offset, 0);
        assert_eq!(info.reader_length, 0);
        assert_eq!(info.writer_length, BUFF_SIZE); 

        assert!(rw.try_set_io_busy(true).is_succ());
        assert!(rw.is_io_busy());
        assert!(rw.try_set_io_busy(false).is_succ());
        assert!(!rw.is_io_busy());
    }

    #[test]
    fn read_wrote_overflow_smoke() {
        let _ = env_logger::builder().is_test(true).try_init();

        const BUFF_SIZE: usize = 4usize;
        let Result::Ok(buff) = BuffState::<std::boxed::Box<[u8]>>
            ::try_new(std::boxed::Box::new([0u8; BUFF_SIZE]))
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
                        let x = buff.writer_forward(len);
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
                        let x = buff.reader_forward(len);
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
        assert!(buff.writer_forward(BUFF_SIZE - 2).is_ok());
        let try_read = buff.try_read(BUFF_SIZE);
        assert!(try_read.is_ok());
        let buf = try_read.ok().unwrap();
        assert!(buff.reader_forward(buf.len()).is_ok());
    }

    fn writer_<P, T, O>(s: Arc<BuffState<P, T, O>>)
    where
        P: BorrowMut<[T]>,
        T: funty::Unsigned + TryFrom<usize> + Copy,
        O: TrCmpxchOrderings,
    {
        let capacity = s.capacity();
        let mut step = 1usize;
        let mut c = 0usize;
        loop {
            if step > capacity {
                break;
            }
            let source = Owned::new_slice(
                step,
                |u| { let Result::Ok(x) = T::try_from(u) else { panic!() }; x },
                CoreAlloc::new(),
            );
            let mut wrote_len = 0usize;
            loop {
                let split = source.split_at(wrote_len);
                let src = split.1;
                match s.try_write(src.len()) {
                    Result::Ok(dual) => {
                        for mut p in dual.into_iter() {
                            let dst = unsafe { p.as_mut() };
                            let len = dst.len();
                            assert!(len <= src.len());
                            dst.clone_from_slice(src.split_at(len).0);
                            wrote_len += len;
                            c += len;
                            let x = s.writer_forward(len);
                            assert!(x.is_ok());
                        }
                        if wrote_len == source.len() {
                            // std::println!("writer #{step}: {:?} ({})", source.as_ref(), *s);
                            break;
                        }
                    },
                    Result::Err(TxError::Stuffed(_)) => continue,
                    Result::Err(e) => panic!(
                        "writer_: step({step}), {:?} - {:?}\n{e:?}",
                        split.0, split.1
                    ),
                }
            }
            if c >= step {
                step += 1;
                c = 0usize;
            }
        }
        s.mark_producer_closed();
        log::trace!("writer exits")
    }

    fn reader_<P, T, O>(s: Arc<BuffState<P, T, O>>)
    where
        P: BorrowMut<[T]>,
        T: funty::Unsigned + TryInto<usize> + Copy,
        O: TrCmpxchOrderings,
    {
        let capacity = s.capacity();
        let mut step = capacity;
        let mut c = 0usize;
        let mut span_length = 1usize;
        let mut span_offset = 0usize;
        loop {
            if step == 0usize {
                break;
            }
            let mut target = Owned::new_slice(
                step,
                |_| T::ZERO,
                CoreAlloc::new(),
            );
            let mut read_len = 0usize;
            loop {
                let split = target.split_at_mut(read_len);
                let dst: &mut [T] = split.1;
                match s.try_read(dst.len()) {
                    Result::Ok(dual) => {
                        for p in dual.into_iter() {
                            let src = unsafe { p.as_ref() };
                            let len = src.len();
                            assert!(len <= dst.len());
                            dst[..len].clone_from_slice(src);
                            read_len += len;
                            c += len;
                            let x = s.reader_forward(len);
                            assert!(x.is_ok());
                        }
                        if read_len == target.len() { break; }
                    },
                    Result::Err(RxError::Drained(_)) => continue,
                    Result::Err(RxError::Closing) => break,
                    Result::Err(e) => panic!(
                        "reader_: step({step}), {:?} - {:?}\n{e:?}",
                        split.0, split.1,
                    ),
                }
            }
            // std::println!("reader #{step}: {:?} ({})", target, *s);
            for (u, x) in target.iter().enumerate() {
                let v = span_offset;
                let Result::Ok(x) = (*x).try_into() else {
                    panic!()
                };
                assert_eq!(v, x, "#{u}: v({v}) != x({x})");
                span_offset += 1;
                if span_offset == span_length {
                    // std::println!("reader done validating span_length({span_length})");
                    span_length += 1;
                    span_offset = 0;
                }
            }
            if c >= step {
                step -= 1;
                c = 0usize;
            }
        }
        s.mark_consumer_closed();
        log::trace!("reader exits")
    }

    #[test]
    fn u8_read_write_concurrent_smoke() {
        const BUFF_SIZE: u8 = u8::MAX;
        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(state) = BuffState::<Owned<[u8], CoreAlloc>>::try_new(
            Owned::new_slice(
                usize::from(BUFF_SIZE),
                |_| 0u8,
                CoreAlloc::new(),
            ))
        else {
            panic!()
        };
        let s = Arc::new(state);
        let s_cloned = s.clone();
        let writer_handle = std::thread::spawn(move || writer_(s_cloned));
        let reader_handle = std::thread::spawn(move || reader_(s));
        let w = writer_handle.join();
        let r = reader_handle.join();
        assert!(w.is_ok());
        assert!(r.is_ok());
    }

    #[test]
    fn u16_read_write_concurrent_smoke() {
        const BUFF_SIZE: u16 = 1024u16;
        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(state) = BuffState::<Owned<[u16], CoreAlloc>, u16>
            ::try_new(Owned::new_slice(
                usize::from(BUFF_SIZE),
                |_| 0u16,
                CoreAlloc::new(),
            ))
        else {
            panic!()
        };
        let s = Arc::new(state);
        let s_cloned = s.clone();
        let writer_handle = std::thread::spawn(move || writer_(s_cloned));
        let reader_handle = std::thread::spawn(move || reader_(s));
        let w = writer_handle.join();
        let r = reader_handle.join();
        assert!(w.is_ok());
        assert!(r.is_ok());
    }

    #[test]
    fn u32_read_write_concurrent_smoke() {
        const BUFF_SIZE: u32 = 1024u32;
        let _ = env_logger::builder().is_test(true).try_init();

        let Result::Ok(state) = BuffState::<Owned<[u32], CoreAlloc>, u32>
            ::try_new(Owned::new_slice(
                usize::try_from(BUFF_SIZE).unwrap(),
                |_| 0u32,
                CoreAlloc::new(),
            ))
        else {
            panic!()
        };
        let s = Arc::new(state);
        let s_cloned = s.clone();
        let writer_handle = std::thread::spawn(move || writer_(s_cloned));
        let reader_handle = std::thread::spawn(move || reader_(s));
        let w = writer_handle.join();
        let r = reader_handle.join();
        assert!(w.is_ok());
        assert!(r.is_ok());
    }
}
