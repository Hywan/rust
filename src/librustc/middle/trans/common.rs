// Copyright 2012-2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Code that is useful in various trans modules.


use driver::session;
use driver::session::Session;
use lib::llvm::{ValueRef, BasicBlockRef, BuilderRef};
use lib::llvm::{True, False, Bool};
use lib::llvm::{llvm};
use lib;
use middle::lang_items::LangItem;
use middle::trans::base;
use middle::trans::build;
use middle::trans::datum;
use middle::trans::glue;
use middle::trans::write_guard;
use middle::trans::debuginfo;
use middle::ty::substs;
use middle::ty;
use middle::typeck;
use middle::borrowck::root_map_key;
use util::ppaux::{Repr};

use middle::trans::type_::Type;

use std::c_str::ToCStr;
use std::cast::transmute;
use std::cast;
use std::hashmap::{HashMap};
use std::libc::{c_uint, c_longlong, c_ulonglong, c_char};
use std::vec;
use syntax::ast::{Name,Ident};
use syntax::ast_map::{path, path_elt, path_pretty_name};
use syntax::codemap::Span;
use syntax::parse::token;
use syntax::{ast, ast_map};

pub use middle::trans::context::CrateContext;

pub fn gensym_name(name: &str) -> (Ident, path_elt) {
    let name = token::gensym(name);
    let ident = Ident::new(name);
    (ident, path_pretty_name(ident, name as u64))
}

pub struct tydesc_info {
    ty: ty::t,
    tydesc: ValueRef,
    size: ValueRef,
    align: ValueRef,
    borrow_offset: ValueRef,
    name: ValueRef,
    take_glue: Option<ValueRef>,
    drop_glue: Option<ValueRef>,
    free_glue: Option<ValueRef>,
    visit_glue: Option<ValueRef>
}

/*
 * A note on nomenclature of linking: "extern", "foreign", and "upcall".
 *
 * An "extern" is an LLVM symbol we wind up emitting an undefined external
 * reference to. This means "we don't have the thing in this compilation unit,
 * please make sure you link it in at runtime". This could be a reference to
 * C code found in a C library, or rust code found in a rust crate.
 *
 * Most "externs" are implicitly declared (automatically) as a result of a
 * user declaring an extern _module_ dependency; this causes the rust driver
 * to locate an extern crate, scan its compilation metadata, and emit extern
 * declarations for any symbols used by the declaring crate.
 *
 * A "foreign" is an extern that references C (or other non-rust ABI) code.
 * There is no metadata to scan for extern references so in these cases either
 * a header-digester like bindgen, or manual function prototypes, have to
 * serve as declarators. So these are usually given explicitly as prototype
 * declarations, in rust code, with ABI attributes on them noting which ABI to
 * link via.
 *
 * An "upcall" is a foreign call generated by the compiler (not corresponding
 * to any user-written call in the code) into the runtime library, to perform
 * some helper task such as bringing a task to life, allocating memory, etc.
 *
 */

pub struct Stats {
    n_static_tydescs: uint,
    n_glues_created: uint,
    n_null_glues: uint,
    n_real_glues: uint,
    n_fns: uint,
    n_monos: uint,
    n_inlines: uint,
    n_closures: uint,
    n_llvm_insns: uint,
    llvm_insn_ctxt: ~[~str],
    llvm_insns: HashMap<~str, uint>,
    fn_stats: ~[(~str, uint, uint)] // (ident, time-in-ms, llvm-instructions)
}

pub struct BuilderRef_res {
    B: BuilderRef,
}

impl Drop for BuilderRef_res {
    fn drop(&self) {
        unsafe {
            llvm::LLVMDisposeBuilder(self.B);
        }
    }
}

pub fn BuilderRef_res(B: BuilderRef) -> BuilderRef_res {
    BuilderRef_res {
        B: B
    }
}

pub type ExternMap = HashMap<~str, ValueRef>;

// Types used for llself.
pub struct ValSelfData {
    v: ValueRef,
    t: ty::t,
    is_copy: bool,
}

// Here `self_ty` is the real type of the self parameter to this method. It
// will only be set in the case of default methods.
pub struct param_substs {
    tys: ~[ty::t],
    self_ty: Option<ty::t>,
    vtables: Option<typeck::vtable_res>,
    self_vtables: Option<typeck::vtable_param_res>
}

impl param_substs {
    pub fn validate(&self) {
        for t in self.tys.iter() { assert!(!ty::type_needs_infer(*t)); }
        for t in self.self_ty.iter() { assert!(!ty::type_needs_infer(*t)); }
    }
}

fn param_substs_to_str(this: &param_substs, tcx: ty::ctxt) -> ~str {
    fmt!("param_substs {tys:%s, vtables:%s}",
         this.tys.repr(tcx),
         this.vtables.repr(tcx))
}

impl Repr for param_substs {
    fn repr(&self, tcx: ty::ctxt) -> ~str {
        param_substs_to_str(self, tcx)
    }
}

// Function context.  Every LLVM function we create will have one of
// these.
pub struct FunctionContext {
    // The ValueRef returned from a call to llvm::LLVMAddFunction; the
    // address of the first instruction in the sequence of
    // instructions for this function that will go in the .text
    // section of the executable we're generating.
    llfn: ValueRef,

    // The implicit environment argument that arrives in the function we're
    // creating.
    llenv: ValueRef,

    // The place to store the return value. If the return type is immediate,
    // this is an alloca in the function. Otherwise, it's the hidden first
    // parameter to the function. After function construction, this should
    // always be Some.
    llretptr: Option<ValueRef>,

    entry_bcx: Option<@mut Block>,

    // These elements: "hoisted basic blocks" containing
    // administrative activities that have to happen in only one place in
    // the function, due to LLVM's quirks.
    // A marker for the place where we want to insert the function's static
    // allocas, so that LLVM will coalesce them into a single alloca call.
    alloca_insert_pt: Option<ValueRef>,
    llreturn: Option<BasicBlockRef>,
    // The 'self' value currently in use in this function, if there
    // is one.
    //
    // NB: This is the type of the self *variable*, not the self *type*. The
    // self type is set only for default methods, while the self variable is
    // set for all methods.
    llself: Option<ValSelfData>,
    // The a value alloca'd for calls to upcalls.rust_personality. Used when
    // outputting the resume instruction.
    personality: Option<ValueRef>,

    // True if the caller expects this fn to use the out pointer to
    // return. Either way, your code should write into llretptr, but if
    // this value is false, llretptr will be a local alloca.
    caller_expects_out_pointer: bool,

    // Maps arguments to allocas created for them in llallocas.
    llargs: @mut HashMap<ast::NodeId, ValueRef>,
    // Maps the def_ids for local variables to the allocas created for
    // them in llallocas.
    lllocals: @mut HashMap<ast::NodeId, ValueRef>,
    // Same as above, but for closure upvars
    llupvars: @mut HashMap<ast::NodeId, ValueRef>,

    // The NodeId of the function, or -1 if it doesn't correspond to
    // a user-defined function.
    id: ast::NodeId,

    // If this function is being monomorphized, this contains the type
    // substitutions used.
    param_substs: Option<@param_substs>,

    // The source span and nesting context where this function comes from, for
    // error reporting and symbol generation.
    span: Option<Span>,
    path: path,

    // This function's enclosing crate context.
    ccx: @mut CrateContext,

    // Used and maintained by the debuginfo module.
    debug_context: debuginfo::FunctionDebugContext,
}

impl FunctionContext {
    pub fn arg_pos(&self, arg: uint) -> uint {
        if self.caller_expects_out_pointer {
            arg + 2u
        } else {
            arg + 1u
        }
    }

    pub fn out_arg_pos(&self) -> uint {
        assert!(self.caller_expects_out_pointer);
        0u
    }

    pub fn env_arg_pos(&self) -> uint {
        if self.caller_expects_out_pointer {
            1u
        } else {
            0u
        }
    }

    pub fn cleanup(&mut self) {
        unsafe {
            llvm::LLVMInstructionEraseFromParent(self.alloca_insert_pt.unwrap());
        }
        // Remove the cycle between fcx and bcx, so memory can be freed
        self.entry_bcx = None;
    }

    pub fn get_llreturn(&mut self) -> BasicBlockRef {
        if self.llreturn.is_none() {
            self.llreturn = Some(base::mk_return_basic_block(self.llfn));
        }

        self.llreturn.unwrap()
    }
}

pub fn warn_not_to_commit(ccx: &mut CrateContext, msg: &str) {
    if !ccx.do_not_commit_warning_issued {
        ccx.do_not_commit_warning_issued = true;
        ccx.sess.warn(msg.to_str() + " -- do not commit like this!");
    }
}

// Heap selectors. Indicate which heap something should go on.
#[deriving(Eq)]
pub enum heap {
    heap_managed,
    heap_managed_unique,
    heap_exchange,
    heap_exchange_closure
}

#[deriving(Clone, Eq)]
pub enum cleantype {
    normal_exit_only,
    normal_exit_and_unwind
}

pub enum cleanup {
    clean(@fn(@mut Block) -> @mut Block, cleantype),
    clean_temp(ValueRef, @fn(@mut Block) -> @mut Block, cleantype),
}

// Can't use deriving(Clone) because of the managed closure.
impl Clone for cleanup {
    fn clone(&self) -> cleanup {
        match *self {
            clean(f, ct) => clean(f, ct),
            clean_temp(v, f, ct) => clean_temp(v, f, ct),
        }
    }
}

// Used to remember and reuse existing cleanup paths
// target: none means the path ends in an resume instruction
#[deriving(Clone)]
pub struct cleanup_path {
    target: Option<BasicBlockRef>,
    size: uint,
    dest: BasicBlockRef
}

pub fn shrink_scope_clean(scope_info: &mut ScopeInfo, size: uint) {
    scope_info.landing_pad = None;
    scope_info.cleanup_paths = scope_info.cleanup_paths.iter()
            .take_while(|&cu| cu.size <= size).map(|&x|x).collect();
}

pub fn grow_scope_clean(scope_info: &mut ScopeInfo) {
    scope_info.landing_pad = None;
}

pub fn cleanup_type(cx: ty::ctxt, ty: ty::t) -> cleantype {
    if ty::type_needs_unwind_cleanup(cx, ty) {
        normal_exit_and_unwind
    } else {
        normal_exit_only
    }
}

pub fn add_clean(bcx: @mut Block, val: ValueRef, t: ty::t) {
    if !ty::type_needs_drop(bcx.tcx(), t) { return; }

    debug!("add_clean(%s, %s, %s)", bcx.to_str(), bcx.val_to_str(val), t.repr(bcx.tcx()));

    let cleanup_type = cleanup_type(bcx.tcx(), t);
    do in_scope_cx(bcx, None) |scope_info| {
        scope_info.cleanups.push(clean(|a| glue::drop_ty(a, val, t), cleanup_type));
        grow_scope_clean(scope_info);
    }
}

pub fn add_clean_temp_immediate(cx: @mut Block, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(cx.tcx(), ty) { return; }
    debug!("add_clean_temp_immediate(%s, %s, %s)",
           cx.to_str(), cx.val_to_str(val),
           ty.repr(cx.tcx()));
    let cleanup_type = cleanup_type(cx.tcx(), ty);
    do in_scope_cx(cx, None) |scope_info| {
        scope_info.cleanups.push(
            clean_temp(val, |a| glue::drop_ty_immediate(a, val, ty),
                       cleanup_type));
        grow_scope_clean(scope_info);
    }
}

pub fn add_clean_temp_mem(bcx: @mut Block, val: ValueRef, t: ty::t) {
    add_clean_temp_mem_in_scope_(bcx, None, val, t);
}

pub fn add_clean_temp_mem_in_scope(bcx: @mut Block,
                                   scope_id: ast::NodeId,
                                   val: ValueRef,
                                   t: ty::t) {
    add_clean_temp_mem_in_scope_(bcx, Some(scope_id), val, t);
}

pub fn add_clean_temp_mem_in_scope_(bcx: @mut Block, scope_id: Option<ast::NodeId>,
                                    val: ValueRef, t: ty::t) {
    if !ty::type_needs_drop(bcx.tcx(), t) { return; }
    debug!("add_clean_temp_mem(%s, %s, %s)",
           bcx.to_str(), bcx.val_to_str(val),
           t.repr(bcx.tcx()));
    let cleanup_type = cleanup_type(bcx.tcx(), t);
    do in_scope_cx(bcx, scope_id) |scope_info| {
        scope_info.cleanups.push(clean_temp(val, |a| glue::drop_ty(a, val, t), cleanup_type));
        grow_scope_clean(scope_info);
    }
}
pub fn add_clean_return_to_mut(bcx: @mut Block,
                               scope_id: ast::NodeId,
                               root_key: root_map_key,
                               frozen_val_ref: ValueRef,
                               bits_val_ref: ValueRef,
                               filename_val: ValueRef,
                               line_val: ValueRef) {
    //! When an `@mut` has been frozen, we have to
    //! call the lang-item `return_to_mut` when the
    //! freeze goes out of scope. We need to pass
    //! in both the value which was frozen (`frozen_val`) and
    //! the value (`bits_val_ref`) which was returned when the
    //! box was frozen initially. Here, both `frozen_val_ref` and
    //! `bits_val_ref` are in fact pointers to stack slots.

    debug!("add_clean_return_to_mut(%s, %s, %s)",
           bcx.to_str(),
           bcx.val_to_str(frozen_val_ref),
           bcx.val_to_str(bits_val_ref));
    do in_scope_cx(bcx, Some(scope_id)) |scope_info| {
        scope_info.cleanups.push(
            clean_temp(
                frozen_val_ref,
                |bcx| write_guard::return_to_mut(bcx, root_key, frozen_val_ref, bits_val_ref,
                                                 filename_val, line_val),
                normal_exit_only));
        grow_scope_clean(scope_info);
    }
}
pub fn add_clean_free(cx: @mut Block, ptr: ValueRef, heap: heap) {
    let free_fn = match heap {
      heap_managed | heap_managed_unique => {
        let f: @fn(@mut Block) -> @mut Block = |a| glue::trans_free(a, ptr);
        f
      }
      heap_exchange | heap_exchange_closure => {
        let f: @fn(@mut Block) -> @mut Block = |a| glue::trans_exchange_free(a, ptr);
        f
      }
    };
    do in_scope_cx(cx, None) |scope_info| {
        scope_info.cleanups.push(clean_temp(ptr, free_fn,
                                      normal_exit_and_unwind));
        grow_scope_clean(scope_info);
    }
}

// Note that this only works for temporaries. We should, at some point, move
// to a system where we can also cancel the cleanup on local variables, but
// this will be more involved. For now, we simply zero out the local, and the
// drop glue checks whether it is zero.
pub fn revoke_clean(cx: @mut Block, val: ValueRef) {
    do in_scope_cx(cx, None) |scope_info| {
        let cleanup_pos = scope_info.cleanups.iter().position(
            |cu| match *cu {
                clean_temp(v, _, _) if v == val => true,
                _ => false
            });
        for i in cleanup_pos.iter() {
            scope_info.cleanups =
                vec::append(scope_info.cleanups.slice(0u, *i).to_owned(),
                            scope_info.cleanups.slice(*i + 1u,
                                                      scope_info.cleanups.len()));
            shrink_scope_clean(scope_info, *i);
        }
    }
}

pub fn block_cleanups(bcx: @mut Block) -> ~[cleanup] {
    match bcx.scope {
       None  => ~[],
       Some(inf) => inf.cleanups.clone(),
    }
}

pub struct ScopeInfo {
    parent: Option<@mut ScopeInfo>,
    loop_break: Option<@mut Block>,
    loop_label: Option<Name>,
    // A list of functions that must be run at when leaving this
    // block, cleaning up any variables that were introduced in the
    // block.
    cleanups: ~[cleanup],
    // Existing cleanup paths that may be reused, indexed by destination and
    // cleared when the set of cleanups changes.
    cleanup_paths: ~[cleanup_path],
    // Unwinding landing pad. Also cleared when cleanups change.
    landing_pad: Option<BasicBlockRef>,
    // info about the AST node this scope originated from, if any
    node_info: Option<NodeInfo>,
}

impl ScopeInfo {
    pub fn empty_cleanups(&mut self) -> bool {
        self.cleanups.is_empty()
    }
}

pub trait get_node_info {
    fn info(&self) -> Option<NodeInfo>;
}

impl get_node_info for ast::Expr {
    fn info(&self) -> Option<NodeInfo> {
        Some(NodeInfo {id: self.id,
                       callee_id: self.get_callee_id(),
                       span: self.span})
    }
}

impl get_node_info for ast::Block {
    fn info(&self) -> Option<NodeInfo> {
        Some(NodeInfo {id: self.id,
                       callee_id: None,
                       span: self.span})
    }
}

impl get_node_info for Option<@ast::Expr> {
    fn info(&self) -> Option<NodeInfo> {
        self.chain_ref(|s| s.info())
    }
}

pub struct NodeInfo {
    id: ast::NodeId,
    callee_id: Option<ast::NodeId>,
    span: Span
}

// Basic block context.  We create a block context for each basic block
// (single-entry, single-exit sequence of instructions) we generate from Rust
// code.  Each basic block we generate is attached to a function, typically
// with many basic blocks per function.  All the basic blocks attached to a
// function are organized as a directed graph.
pub struct Block {
    // The BasicBlockRef returned from a call to
    // llvm::LLVMAppendBasicBlock(llfn, name), which adds a basic
    // block to the function pointed to by llfn.  We insert
    // instructions into that block by way of this block context.
    // The block pointing to this one in the function's digraph.
    llbb: BasicBlockRef,
    terminated: bool,
    unreachable: bool,
    parent: Option<@mut Block>,
    // The current scope within this basic block
    scope: Option<@mut ScopeInfo>,
    // Is this block part of a landing pad?
    is_lpad: bool,
    // info about the AST node this block originated from, if any
    node_info: Option<NodeInfo>,
    // The function context for the function to which this block is
    // attached.
    fcx: @mut FunctionContext
}

impl Block {

    pub fn new(llbb: BasicBlockRef,
               parent: Option<@mut Block>,
               is_lpad: bool,
               node_info: Option<NodeInfo>,
               fcx: @mut FunctionContext)
            -> Block {
        Block {
            llbb: llbb,
            terminated: false,
            unreachable: false,
            parent: parent,
            scope: None,
            is_lpad: is_lpad,
            node_info: node_info,
            fcx: fcx
        }
    }

    pub fn ccx(&self) -> @mut CrateContext { self.fcx.ccx }
    pub fn tcx(&self) -> ty::ctxt { self.fcx.ccx.tcx }
    pub fn sess(&self) -> Session { self.fcx.ccx.sess }

    pub fn ident(&self, ident: Ident) -> @str {
        token::ident_to_str(&ident)
    }

    pub fn node_id_to_str(&self, id: ast::NodeId) -> ~str {
        ast_map::node_id_to_str(self.tcx().items, id, self.sess().intr())
    }

    pub fn expr_to_str(&self, e: @ast::Expr) -> ~str {
        e.repr(self.tcx())
    }

    pub fn expr_is_lval(&self, e: &ast::Expr) -> bool {
        ty::expr_is_lval(self.tcx(), self.ccx().maps.method_map, e)
    }

    pub fn expr_kind(&self, e: &ast::Expr) -> ty::ExprKind {
        ty::expr_kind(self.tcx(), self.ccx().maps.method_map, e)
    }

    pub fn def(&self, nid: ast::NodeId) -> ast::Def {
        match self.tcx().def_map.find(&nid) {
            Some(&v) => v,
            None => {
                self.tcx().sess.bug(fmt!(
                    "No def associated with node id %?", nid));
            }
        }
    }

    pub fn val_to_str(&self, val: ValueRef) -> ~str {
        self.ccx().tn.val_to_str(val)
    }

    pub fn llty_str(&self, ty: Type) -> ~str {
        self.ccx().tn.type_to_str(ty)
    }

    pub fn ty_to_str(&self, t: ty::t) -> ~str {
        t.repr(self.tcx())
    }

    pub fn to_str(&self) -> ~str {
        unsafe {
            match self.node_info {
                Some(node_info) => fmt!("[block %d]", node_info.id),
                None => fmt!("[block %x]", transmute(&*self)),
            }
        }
    }
}

pub struct Result {
    bcx: @mut Block,
    val: ValueRef
}

pub fn rslt(bcx: @mut Block, val: ValueRef) -> Result {
    Result {bcx: bcx, val: val}
}

impl Result {
    pub fn unpack(&self, bcx: &mut @mut Block) -> ValueRef {
        *bcx = self.bcx;
        return self.val;
    }
}

pub fn val_ty(v: ValueRef) -> Type {
    unsafe {
        Type::from_ref(llvm::LLVMTypeOf(v))
    }
}

pub fn in_scope_cx(cx: @mut Block, scope_id: Option<ast::NodeId>, f: &fn(si: &mut ScopeInfo)) {
    let mut cur = cx;
    let mut cur_scope = cur.scope;
    loop {
        cur_scope = match cur_scope {
            Some(inf) => match scope_id {
                Some(wanted) => match inf.node_info {
                    Some(NodeInfo { id: actual, _ }) if wanted == actual => {
                        debug!("in_scope_cx: selected cur=%s (cx=%s)",
                               cur.to_str(), cx.to_str());
                        f(inf);
                        return;
                    },
                    _ => inf.parent,
                },
                None => {
                    debug!("in_scope_cx: selected cur=%s (cx=%s)",
                           cur.to_str(), cx.to_str());
                    f(inf);
                    return;
                }
            },
            None => {
                cur = block_parent(cur);
                cur.scope
            }
        }
    }
}

pub fn block_parent(cx: @mut Block) -> @mut Block {
    match cx.parent {
      Some(b) => b,
      None    => cx.sess().bug(fmt!("block_parent called on root block %?",
                                   cx))
    }
}


// Let T be the content of a box @T.  tuplify_box_ty(t) returns the
// representation of @T as a tuple (i.e., the ty::t version of what T_box()
// returns).
pub fn tuplify_box_ty(tcx: ty::ctxt, t: ty::t) -> ty::t {
    let ptr = ty::mk_ptr(
        tcx,
        ty::mt {ty: ty::mk_i8(), mutbl: ast::MutImmutable}
    );
    return ty::mk_tup(tcx, ~[ty::mk_uint(), ty::mk_type(tcx),
                         ptr, ptr,
                         t]);
}

// LLVM constant constructors.
pub fn C_null(t: Type) -> ValueRef {
    unsafe {
        llvm::LLVMConstNull(t.to_ref())
    }
}

pub fn C_undef(t: Type) -> ValueRef {
    unsafe {
        llvm::LLVMGetUndef(t.to_ref())
    }
}

pub fn C_integral(t: Type, u: u64, sign_extend: bool) -> ValueRef {
    unsafe {
        llvm::LLVMConstInt(t.to_ref(), u, sign_extend as Bool)
    }
}

pub fn C_floating(s: &str, t: Type) -> ValueRef {
    unsafe {
        do s.with_c_str |buf| {
            llvm::LLVMConstRealOfString(t.to_ref(), buf)
        }
    }
}

pub fn C_nil() -> ValueRef {
    return C_struct([]);
}

pub fn C_bool(val: bool) -> ValueRef {
    C_integral(Type::bool(), val as u64, false)
}

pub fn C_i1(val: bool) -> ValueRef {
    C_integral(Type::i1(), val as u64, false)
}

pub fn C_i32(i: i32) -> ValueRef {
    return C_integral(Type::i32(), i as u64, true);
}

pub fn C_i64(i: i64) -> ValueRef {
    return C_integral(Type::i64(), i as u64, true);
}

pub fn C_int(cx: &CrateContext, i: int) -> ValueRef {
    return C_integral(cx.int_type, i as u64, true);
}

pub fn C_uint(cx: &CrateContext, i: uint) -> ValueRef {
    return C_integral(cx.int_type, i as u64, false);
}

pub fn C_u8(i: uint) -> ValueRef {
    return C_integral(Type::i8(), i as u64, false);
}


// This is a 'c-like' raw string, which differs from
// our boxed-and-length-annotated strings.
pub fn C_cstr(cx: &mut CrateContext, s: @str) -> ValueRef {
    unsafe {
        match cx.const_cstr_cache.find_equiv(&s) {
            Some(&llval) => return llval,
            None => ()
        }

        let sc = do s.as_imm_buf |buf, buflen| {
            llvm::LLVMConstStringInContext(cx.llcx, buf as *c_char, buflen as c_uint, False)
        };

        let gsym = token::gensym("str");
        let g = do fmt!("str%u", gsym).with_c_str |buf| {
            llvm::LLVMAddGlobal(cx.llmod, val_ty(sc).to_ref(), buf)
        };
        llvm::LLVMSetInitializer(g, sc);
        llvm::LLVMSetGlobalConstant(g, True);
        lib::llvm::SetLinkage(g, lib::llvm::InternalLinkage);

        cx.const_cstr_cache.insert(s, g);

        return g;
    }
}

// NB: Do not use `do_spill_noroot` to make this into a constant string, or
// you will be kicked off fast isel. See issue #4352 for an example of this.
pub fn C_estr_slice(cx: &mut CrateContext, s: @str) -> ValueRef {
    unsafe {
        let len = s.len();
        let cs = llvm::LLVMConstPointerCast(C_cstr(cx, s), Type::i8p().to_ref());
        C_struct([cs, C_uint(cx, len)])
    }
}

pub fn C_zero_byte_arr(size: uint) -> ValueRef {
    unsafe {
        let mut i = 0u;
        let mut elts: ~[ValueRef] = ~[];
        while i < size { elts.push(C_u8(0u)); i += 1u; }
        return llvm::LLVMConstArray(Type::i8().to_ref(),
                                    vec::raw::to_ptr(elts), elts.len() as c_uint);
    }
}

pub fn C_struct(elts: &[ValueRef]) -> ValueRef {
    unsafe {
        do elts.as_imm_buf |ptr, len| {
            llvm::LLVMConstStructInContext(base::task_llcx(), ptr, len as c_uint, False)
        }
    }
}

pub fn C_packed_struct(elts: &[ValueRef]) -> ValueRef {
    unsafe {
        do elts.as_imm_buf |ptr, len| {
            llvm::LLVMConstStructInContext(base::task_llcx(), ptr, len as c_uint, True)
        }
    }
}

pub fn C_named_struct(T: Type, elts: &[ValueRef]) -> ValueRef {
    unsafe {
        do elts.as_imm_buf |ptr, len| {
            llvm::LLVMConstNamedStruct(T.to_ref(), ptr, len as c_uint)
        }
    }
}

pub fn C_array(ty: Type, elts: &[ValueRef]) -> ValueRef {
    unsafe {
        return llvm::LLVMConstArray(ty.to_ref(), vec::raw::to_ptr(elts), elts.len() as c_uint);
    }
}

pub fn C_bytes(bytes: &[u8]) -> ValueRef {
    unsafe {
        let ptr = cast::transmute(vec::raw::to_ptr(bytes));
        return llvm::LLVMConstStringInContext(base::task_llcx(), ptr, bytes.len() as c_uint, True);
    }
}

pub fn get_param(fndecl: ValueRef, param: uint) -> ValueRef {
    unsafe {
        llvm::LLVMGetParam(fndecl, param as c_uint)
    }
}

pub fn const_get_elt(cx: &CrateContext, v: ValueRef, us: &[c_uint])
                  -> ValueRef {
    unsafe {
        let r = do us.as_imm_buf |p, len| {
            llvm::LLVMConstExtractValue(v, p, len as c_uint)
        };

        debug!("const_get_elt(v=%s, us=%?, r=%s)",
               cx.tn.val_to_str(v), us, cx.tn.val_to_str(r));

        return r;
    }
}

pub fn is_const(v: ValueRef) -> bool {
    unsafe {
        llvm::LLVMIsConstant(v) == True
    }
}

pub fn const_to_int(v: ValueRef) -> c_longlong {
    unsafe {
        llvm::LLVMConstIntGetSExtValue(v)
    }
}

pub fn const_to_uint(v: ValueRef) -> c_ulonglong {
    unsafe {
        llvm::LLVMConstIntGetZExtValue(v)
    }
}

pub fn is_undef(val: ValueRef) -> bool {
    unsafe {
        llvm::LLVMIsUndef(val) != False
    }
}

pub fn is_null(val: ValueRef) -> bool {
    unsafe {
        llvm::LLVMIsNull(val) != False
    }
}

// Used to identify cached monomorphized functions and vtables
#[deriving(Eq,IterBytes)]
pub enum mono_param_id {
    mono_precise(ty::t, Option<@~[mono_id]>),
    mono_any,
    mono_repr(uint /* size */,
              uint /* align */,
              MonoDataClass,
              datum::DatumMode),
}

#[deriving(Eq,IterBytes)]
pub enum MonoDataClass {
    MonoBits,    // Anything not treated differently from arbitrary integer data
    MonoNonNull, // Non-null pointers (used for optional-pointer optimization)
    // FIXME(#3547)---scalars and floats are
    // treated differently in most ABIs.  But we
    // should be doing something more detailed
    // here.
    MonoFloat
}

pub fn mono_data_classify(t: ty::t) -> MonoDataClass {
    match ty::get(t).sty {
        ty::ty_float(_) => MonoFloat,
        ty::ty_rptr(*) | ty::ty_uniq(*) |
        ty::ty_box(*) | ty::ty_opaque_box(*) |
        ty::ty_estr(ty::vstore_uniq) | ty::ty_evec(_, ty::vstore_uniq) |
        ty::ty_estr(ty::vstore_box) | ty::ty_evec(_, ty::vstore_box) |
        ty::ty_bare_fn(*) => MonoNonNull,
        // Is that everything?  Would closures or slices qualify?
        _ => MonoBits
    }
}


#[deriving(Eq,IterBytes)]
pub struct mono_id_ {
    def: ast::DefId,
    params: ~[mono_param_id]
}

pub type mono_id = @mono_id_;

pub fn umax(cx: @mut Block, a: ValueRef, b: ValueRef) -> ValueRef {
    let cond = build::ICmp(cx, lib::llvm::IntULT, a, b);
    return build::Select(cx, cond, b, a);
}

pub fn umin(cx: @mut Block, a: ValueRef, b: ValueRef) -> ValueRef {
    let cond = build::ICmp(cx, lib::llvm::IntULT, a, b);
    return build::Select(cx, cond, a, b);
}

pub fn align_to(cx: @mut Block, off: ValueRef, align: ValueRef) -> ValueRef {
    let mask = build::Sub(cx, align, C_int(cx.ccx(), 1));
    let bumped = build::Add(cx, off, mask);
    return build::And(cx, bumped, build::Not(cx, mask));
}

pub fn path_str(sess: session::Session, p: &[path_elt]) -> ~str {
    let mut r = ~"";
    let mut first = true;
    for e in p.iter() {
        match *e {
            ast_map::path_name(s) | ast_map::path_mod(s) |
            ast_map::path_pretty_name(s, _) => {
                if first {
                    first = false
                } else {
                    r.push_str("::")
                }
                r.push_str(sess.str_of(s));
            }
        }
    }
    r
}

pub fn monomorphize_type(bcx: @mut Block, t: ty::t) -> ty::t {
    match bcx.fcx.param_substs {
        Some(substs) => {
            ty::subst_tps(bcx.tcx(), substs.tys, substs.self_ty, t)
        }
        _ => {
            assert!(!ty::type_has_params(t));
            assert!(!ty::type_has_self(t));
            t
        }
    }
}

pub fn node_id_type(bcx: @mut Block, id: ast::NodeId) -> ty::t {
    let tcx = bcx.tcx();
    let t = ty::node_id_to_type(tcx, id);
    monomorphize_type(bcx, t)
}

pub fn expr_ty(bcx: @mut Block, ex: &ast::Expr) -> ty::t {
    node_id_type(bcx, ex.id)
}

pub fn expr_ty_adjusted(bcx: @mut Block, ex: &ast::Expr) -> ty::t {
    let tcx = bcx.tcx();
    let t = ty::expr_ty_adjusted(tcx, ex);
    monomorphize_type(bcx, t)
}

pub fn node_id_type_params(bcx: @mut Block, id: ast::NodeId) -> ~[ty::t] {
    let tcx = bcx.tcx();
    let params = ty::node_id_to_type_params(tcx, id);

    if !params.iter().all(|t| !ty::type_needs_infer(*t)) {
        bcx.sess().bug(
            fmt!("Type parameters for node %d include inference types: %s",
                 id, params.map(|t| bcx.ty_to_str(*t)).connect(",")));
    }

    match bcx.fcx.param_substs {
      Some(substs) => {
        do params.iter().map |t| {
            ty::subst_tps(tcx, substs.tys, substs.self_ty, *t)
        }.collect()
      }
      _ => params
    }
}

pub fn node_vtables(bcx: @mut Block, id: ast::NodeId)
                 -> Option<typeck::vtable_res> {
    let raw_vtables = bcx.ccx().maps.vtable_map.find(&id);
    raw_vtables.map_move(|vts| resolve_vtables_in_fn_ctxt(bcx.fcx, *vts))
}

// Apply the typaram substitutions in the FunctionContext to some
// vtables. This should eliminate any vtable_params.
pub fn resolve_vtables_in_fn_ctxt(fcx: &FunctionContext, vts: typeck::vtable_res)
    -> typeck::vtable_res {
    resolve_vtables_under_param_substs(fcx.ccx.tcx,
                                       fcx.param_substs,
                                       vts)
}

pub fn resolve_vtables_under_param_substs(tcx: ty::ctxt,
                                          param_substs: Option<@param_substs>,
                                          vts: typeck::vtable_res)
    -> typeck::vtable_res {
    @vts.iter().map(|ds|
      resolve_param_vtables_under_param_substs(tcx,
                                               param_substs,
                                               *ds))
        .collect()
}

pub fn resolve_param_vtables_under_param_substs(
    tcx: ty::ctxt,
    param_substs: Option<@param_substs>,
    ds: typeck::vtable_param_res)
    -> typeck::vtable_param_res {
    @ds.iter().map(
        |d| resolve_vtable_under_param_substs(tcx,
                                              param_substs,
                                              d))
        .collect()
}



pub fn resolve_vtable_under_param_substs(tcx: ty::ctxt,
                                         param_substs: Option<@param_substs>,
                                         vt: &typeck::vtable_origin)
                                         -> typeck::vtable_origin {
    match *vt {
        typeck::vtable_static(trait_id, ref tys, sub) => {
            let tys = match param_substs {
                Some(substs) => {
                    do tys.iter().map |t| {
                        ty::subst_tps(tcx, substs.tys, substs.self_ty, *t)
                    }.collect()
                }
                _ => tys.to_owned()
            };
            typeck::vtable_static(
                trait_id, tys,
                resolve_vtables_under_param_substs(tcx, param_substs, sub))
        }
        typeck::vtable_param(n_param, n_bound) => {
            match param_substs {
                Some(substs) => {
                    find_vtable(tcx, substs, n_param, n_bound)
                }
                _ => {
                    tcx.sess.bug(fmt!(
                        "resolve_vtable_under_param_substs: asked to lookup \
                         but no vtables in the fn_ctxt!"))
                }
            }
        }
    }
}

pub fn find_vtable(tcx: ty::ctxt,
                   ps: &param_substs,
                   n_param: typeck::param_index,
                   n_bound: uint)
                   -> typeck::vtable_origin {
    debug!("find_vtable(n_param=%?, n_bound=%u, ps=%s)",
           n_param, n_bound, ps.repr(tcx));

    let param_bounds = match n_param {
        typeck::param_self => ps.self_vtables.expect("self vtables missing"),
        typeck::param_numbered(n) => {
            let tables = ps.vtables
                .expect("vtables missing where they are needed");
            tables[n]
        }
    };
    param_bounds[n_bound].clone()
}

pub fn dummy_substs(tps: ~[ty::t]) -> ty::substs {
    substs {
        regions: ty::ErasedRegions,
        self_ty: None,
        tps: tps
    }
}

pub fn filename_and_line_num_from_span(bcx: @mut Block,
                                       span: Span) -> (ValueRef, ValueRef) {
    let loc = bcx.sess().parse_sess.cm.lookup_char_pos(span.lo);
    let filename_cstr = C_cstr(bcx.ccx(), loc.file.name);
    let filename = build::PointerCast(bcx, filename_cstr, Type::i8p());
    let line = C_int(bcx.ccx(), loc.line as int);
    (filename, line)
}

// Casts a Rust bool value to an i1.
pub fn bool_to_i1(bcx: @mut Block, llval: ValueRef) -> ValueRef {
    build::ICmp(bcx, lib::llvm::IntNE, llval, C_bool(false))
}

pub fn langcall(bcx: @mut Block, span: Option<Span>, msg: &str,
                li: LangItem) -> ast::DefId {
    match bcx.tcx().lang_items.require(li) {
        Ok(id) => id,
        Err(s) => {
            let msg = fmt!("%s %s", msg, s);
            match span {
                Some(span) => { bcx.tcx().sess.span_fatal(span, msg); }
                None => { bcx.tcx().sess.fatal(msg); }
            }
        }
    }
}
