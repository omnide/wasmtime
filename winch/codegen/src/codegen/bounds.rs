//! Exposes heap bounds checks functionality for WebAssembly.
//! Bounds checks in WebAssembly are critical for safety, so extreme caution is
//! recommended when working on this area of Winch.
use super::env::{HeapData, HeapStyle};
use crate::{
    abi::ABI,
    codegen::CodeGenContext,
    isa::reg::Reg,
    masm::{IntCmpKind, MacroAssembler, OperandSize, RegImm, TrapCode},
    stack::TypedReg,
};

/// A newtype to represent an immediate offset argument for a heap access.
#[derive(Debug, Copy, Clone)]
pub(crate) struct ImmOffset(u32);

impl ImmOffset {
    /// Construct an [ImmOffset] from a u32.
    pub fn from_u32(raw: u32) -> Self {
        Self(raw)
    }

    /// Return the underlying u32 value.
    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

/// An enum to represent the heap bounds.
#[derive(Debug, Copy, Clone)]
pub(crate) enum Bounds {
    /// Static, known ahead-of-time.
    Static(u64),
    /// Dynamic. Loaded at runtime.
    Dynamic(TypedReg),
}

impl Bounds {
    /// Construct a [Bounds] from a [TypedReg].
    pub fn from_typed_reg(tr: TypedReg) -> Self {
        Self::Dynamic(tr)
    }

    /// Construct a [Bounds] from a u64.
    pub fn from_u64(raw: u64) -> Self {
        Self::Static(raw)
    }

    /// Return the underlying [TypedReg] value.
    pub fn as_typed_reg(&self) -> TypedReg {
        match self {
            Self::Dynamic(tr) => *tr,
            _ => panic!(),
        }
    }

    /// Return the underlying u64 value.
    pub fn as_u64(&self) -> u64 {
        match self {
            Self::Static(v) => *v,
            _ => panic!(),
        }
    }
}

/// A newtype to represent a heap access index via a [TypedReg].
#[derive(Debug, Copy, Clone)]
pub(crate) struct Index(TypedReg);

impl Index {
    /// Construct an [Index] from a [TypedReg].
    pub fn from_typed_reg(tr: TypedReg) -> Self {
        Self(tr)
    }

    /// Return the underlying
    pub fn as_typed_reg(&self) -> TypedReg {
        self.0
    }
}

/// Loads the bounds of the dynamic heap.
pub(crate) fn load_dynamic_heap_bounds<M>(
    context: &mut CodeGenContext,
    masm: &mut M,
    heap: &HeapData,
    ptr_size: OperandSize,
) -> Bounds
where
    M: MacroAssembler,
{
    let dst = context.any_gpr(masm);
    match (heap.max_size, &heap.style) {
        // Constant size, no need to perform a load.
        (Some(max_size), HeapStyle::Dynamic) if heap.min_size == max_size => {
            masm.mov(RegImm::i64(max_size as i64), dst, ptr_size)
        }
        (_, HeapStyle::Dynamic) => {
            let scratch = <M::ABI as ABI>::scratch_reg();
            let base = if let Some(offset) = heap.import_from {
                let addr = masm.address_at_vmctx(offset);
                masm.load_ptr(addr, scratch);
                scratch
            } else {
                <M::ABI as ABI>::vmctx_reg()
            };
            let addr = masm.address_at_reg(base, heap.current_length_offset);
            masm.load_ptr(addr, dst);
        }
        (_, HeapStyle::Static { .. }) => unreachable!("Loading dynamic bounds of a static heap"),
    }

    Bounds::from_typed_reg(TypedReg::new(heap.ty, dst))
}

/// This function ensures the following:
/// * The immediate offset and memory access size fit in a single u64. Given:
/// that the memory access size is a `u8`, we must guarantee that the immediate
/// offset will fit in a `u32`, making the result of their addition fit in a u64
/// and overflow safe.
/// * Adjust the base index to account for the immediate offset via an unsigned
/// addition and check for overflow in case the previous condition is not met.
#[inline]
pub(crate) fn ensure_index_and_offset<M: MacroAssembler>(
    masm: &mut M,
    index: Index,
    offset: u64,
    ptr_size: OperandSize,
) -> ImmOffset {
    match u32::try_from(offset) {
        // If the immediate offset fits in a u32, then we simply return.
        Ok(offs) => ImmOffset::from_u32(offs),
        // Else we adjust the index to be index = index + offset, including an
        // overflow check, and return 0 as the offset.
        Err(_) => {
            masm.checked_uadd(
                index.as_typed_reg().into(),
                index.as_typed_reg().into(),
                RegImm::i64(offset as i64),
                ptr_size,
                TrapCode::HeapOutOfBounds,
            );

            ImmOffset::from_u32(0)
        }
    }
}

/// Performs the out-of-bounds check and returns the heap address if the access
/// criteria is in bounds.
pub(crate) fn load_heap_addr_checked<M, F>(
    masm: &mut M,
    context: &mut CodeGenContext,
    ptr_size: OperandSize,
    heap: &HeapData,
    enable_spectre_mitigation: bool,
    bounds: Bounds,
    index: Index,
    offset: ImmOffset,
    mut emit_check_condition: F,
) -> Reg
where
    M: MacroAssembler,
    F: FnMut(&mut M, Bounds, Index) -> IntCmpKind,
{
    let cmp_kind = emit_check_condition(masm, bounds, index);

    masm.trapif(cmp_kind, TrapCode::HeapOutOfBounds);
    let addr = context.any_gpr(masm);

    load_heap_addr_unchecked(masm, heap, index, offset, addr, ptr_size);
    if !enable_spectre_mitigation {
        addr
    } else {
        // Conditionally assign 0 to the register holding the base address if
        // the comparison kind is met.
        let tmp = context.any_gpr(masm);
        masm.mov(RegImm::i64(0), tmp, ptr_size);
        let cmp_kind = emit_check_condition(masm, bounds, index);
        masm.cmov(tmp, addr, cmp_kind, ptr_size);
        context.free_reg(tmp);
        addr
    }
}

/// Load the requested heap address into the specified destination register.
/// This function doesn't perform any bounds checks and assumes the caller
/// performed the right checks.
pub(crate) fn load_heap_addr_unchecked<M>(
    masm: &mut M,
    heap: &HeapData,
    index: Index,
    offset: ImmOffset,
    dst: Reg,
    ptr_size: OperandSize,
) where
    M: MacroAssembler,
{
    let base = if let Some(offset) = heap.import_from {
        // If the WebAssembly memory is imported, load the address into
        // the scratch register.
        let scratch = <M::ABI as ABI>::scratch_reg();
        masm.load_ptr(masm.address_at_vmctx(offset), scratch);
        scratch
    } else {
        // Else if the WebAssembly memory is defined in the current module,
        // simply use the `VMContext` as the base for subsequent operations.
        <M::ABI as ABI>::vmctx_reg()
    };

    // Load the base of the memory into the `addr` register.
    masm.load_ptr(masm.address_at_reg(base, heap.offset), dst);
    // Start by adding the index to the heap base addr.
    let index_reg = index.as_typed_reg().reg;
    masm.add(dst, dst, index_reg.into(), ptr_size);

    if offset.as_u32() > 0 {
        masm.add(dst, dst, RegImm::i64(offset.as_u32() as i64), ptr_size);
    }
}
