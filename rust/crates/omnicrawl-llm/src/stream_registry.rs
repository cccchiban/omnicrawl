//! 当前回合的可取消模型流与外部资源注册表。
//!
//! 语义基准是 Python 侧 `omnicrawl/llm/stream_registry.py`：注册进来的流与外部资源按
//! 「对象身份」归属到某个可独立取消的回合，取消时只关闭该回合的注册项。Python 用 `is`
//! 比较对象身份、用 `ContextVar` 保存当前回合归属；这里用 `Arc` 指针身份与线程局部
//! 作用域栈表达同一件事。
//!
//! 与 Python 的两处已知差异（都写进 crate README）：作用域是线程局部的，跨线程注册要像
//! Python 的「owner 由调用线程解析」那样显式传 `owner`；关闭动作 panic 不会像 Python 的
//! `except Exception` 那样被吞掉。

use std::cell::RefCell;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

/// 关闭动作：显式 `close_callback` 与资源自带关闭都归一到这个形状。
pub type CloseAction = Arc<dyn Fn() + Send + Sync>;

/// 回合归属令牌。相等性只看 `Arc` 指针，对应 Python 的 `is` 比较。
#[derive(Clone)]
pub struct ScopeOwner(Arc<()>);

impl ScopeOwner {
    pub fn new() -> Self {
        Self(Arc::new(()))
    }

    /// 是否为同一归属（Python 的 `entry.owner is owner`）。
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Default for ScopeOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ScopeOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ScopeOwner")
            .field(&Arc::as_ptr(&self.0))
            .finish()
    }
}

/// 一个可取消资源句柄。`Arc` 指针即身份；自带关闭动作对应 Python 里资源对象的 `close()`。
///
/// 没有关闭动作的句柄对应 Python 里 `object()` 这类没有 `close` 的对象：注册没问题，
/// 关闭阶段不会被计入成功数。
pub struct CancelHandle {
    close: Option<CloseAction>,
}

impl CancelHandle {
    /// 没有自带关闭动作的句柄。
    pub fn without_close() -> Arc<Self> {
        Arc::new(Self { close: None })
    }

    /// 自带关闭动作的句柄。
    pub fn with_close(close: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            close: Some(Arc::new(close)),
        })
    }

    pub fn has_close(&self) -> bool {
        self.close.is_some()
    }

    fn invoke_close(&self) {
        if let Some(close) = &self.close {
            close();
        }
    }
}

#[derive(Clone)]
struct Entry {
    handle: Arc<CancelHandle>,
    owner: Option<ScopeOwner>,
    close: Option<CloseAction>,
}

/// 回合注册表。Python 侧是模块级全局表；这里既可建独立实例（便于对照与测试），
/// 也可用 [`global`] 取与 Python 同义的全局表。
#[derive(Default)]
pub struct StreamRegistry {
    streams: Mutex<Vec<Entry>>,
    resources: Mutex<Vec<Entry>>,
}

impl StreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个进行中的模型响应流，取消时需要关闭它。
    pub fn register_stream(
        &self,
        handle: Option<&Arc<CancelHandle>>,
        owner: Option<ScopeOwner>,
        close: Option<CloseAction>,
    ) {
        register_in(&self.streams, handle, owner, close);
    }

    /// 注销模型响应流，避免悬挂引用。
    pub fn unregister_stream(&self, handle: &Arc<CancelHandle>) {
        unregister_in(&self.streams, handle);
    }

    /// 关闭指定回合的活跃模型流，返回成功关闭的数量。
    pub fn close_active_streams(&self, owner: Option<&ScopeOwner>) -> usize {
        close_active_in(&self.streams, owner)
    }

    /// 返回当前活跃模型流数量，可按回合归属过滤。
    pub fn active_stream_count(&self, owner: Option<&ScopeOwner>) -> usize {
        count_active_in(&self.streams, owner)
    }

    /// 注册一个可被回合取消的外部资源，例如正在运行的子进程。
    pub fn register_resource(
        &self,
        handle: Option<&Arc<CancelHandle>>,
        owner: Option<ScopeOwner>,
        close: Option<CloseAction>,
    ) {
        register_in(&self.resources, handle, owner, close);
    }

    /// 注销外部资源。
    pub fn unregister_resource(&self, handle: &Arc<CancelHandle>) {
        unregister_in(&self.resources, handle);
    }

    /// 关闭指定回合的可取消外部资源，返回成功关闭的数量。
    pub fn close_active_resources(&self, owner: Option<&ScopeOwner>) -> usize {
        close_active_in(&self.resources, owner)
    }

    /// 返回当前可取消外部资源数量，可按回合归属过滤。
    pub fn active_resource_count(&self, owner: Option<&ScopeOwner>) -> usize {
        count_active_in(&self.resources, owner)
    }
}

/// 与 Python 模块级全局表同义的注册表。
pub fn global() -> &'static StreamRegistry {
    static GLOBAL: OnceLock<StreamRegistry> = OnceLock::new();
    GLOBAL.get_or_init(StreamRegistry::new)
}

/// 注册一个进行中的模型响应流，取消时需要关闭它（全局表）。
pub fn register_stream(
    handle: Option<&Arc<CancelHandle>>,
    owner: Option<ScopeOwner>,
    close: Option<CloseAction>,
) {
    global().register_stream(handle, owner, close);
}

/// 注销模型响应流，避免悬挂引用（全局表）。
pub fn unregister_stream(handle: &Arc<CancelHandle>) {
    global().unregister_stream(handle);
}

/// 关闭指定回合的活跃模型流，返回成功关闭的数量（全局表）。
pub fn close_active_streams(owner: Option<&ScopeOwner>) -> usize {
    global().close_active_streams(owner)
}

/// 返回当前活跃模型流数量，可按回合归属过滤（全局表）。
pub fn active_stream_count(owner: Option<&ScopeOwner>) -> usize {
    global().active_stream_count(owner)
}

/// 注册一个可被回合取消的外部资源（全局表）。
pub fn register_resource(
    handle: Option<&Arc<CancelHandle>>,
    owner: Option<ScopeOwner>,
    close: Option<CloseAction>,
) {
    global().register_resource(handle, owner, close);
}

/// 注销外部资源（全局表）。
pub fn unregister_resource(handle: &Arc<CancelHandle>) {
    global().unregister_resource(handle);
}

/// 关闭指定回合的可取消外部资源，返回成功关闭的数量（全局表）。
pub fn close_active_resources(owner: Option<&ScopeOwner>) -> usize {
    global().close_active_resources(owner)
}

/// 返回当前可取消外部资源数量，可按回合归属过滤（全局表）。
pub fn active_resource_count(owner: Option<&ScopeOwner>) -> usize {
    global().active_resource_count(owner)
}

thread_local! {
    static SCOPE_STACK: RefCell<Vec<ScopeOwner>> = const { RefCell::new(Vec::new()) };
}

/// 把当前线程创建的流与外部资源绑定到一个可独立取消的回合。
///
/// 对应 Python 的 `with stream_scope(owner)`；离开作用域即恢复上一层归属。
#[must_use]
pub struct StreamScope {
    _owner: ScopeOwner,
}

/// 打开一层回合作用域，返回的守卫离开作用域时自动恢复上一层。
pub fn stream_scope(owner: ScopeOwner) -> StreamScope {
    SCOPE_STACK.with(|stack| stack.borrow_mut().push(owner.clone()));
    StreamScope { _owner: owner }
}

impl Drop for StreamScope {
    fn drop(&mut self) {
        SCOPE_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// 返回当前线程的取消归属。
pub fn current_stream_scope() -> Option<ScopeOwner> {
    SCOPE_STACK.with(|stack| stack.borrow().last().cloned())
}

/// 自带回合归属的取消检查器（对应 Python 侧 `cancel_check.stream_owner`）。
pub trait StreamOwnerCarrier {
    fn stream_owner(&self) -> Option<ScopeOwner>;
}

/// 从取消检查器或线程上下文取得资源的取消归属。
pub fn stream_owner_for(cancel_check: Option<&dyn StreamOwnerCarrier>) -> Option<ScopeOwner> {
    cancel_check
        .and_then(StreamOwnerCarrier::stream_owner)
        .or_else(current_stream_scope)
}

/// 迭代模型流并保证正常结束与提前退出都会注销它。
pub struct RegisteredStreamEvents<I> {
    inner: I,
    handle: Arc<CancelHandle>,
}

impl<I: Iterator> Iterator for RegisteredStreamEvents<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl<I> Drop for RegisteredStreamEvents<I> {
    fn drop(&mut self) {
        global().unregister_stream(&self.handle);
    }
}

/// 注册 `handle` 后返回包装迭代器；迭代结束或中途丢弃都会注销。
pub fn registered_stream_events<I: Iterator>(
    handle: Arc<CancelHandle>,
    events: I,
    owner: Option<ScopeOwner>,
    close: Option<CloseAction>,
) -> RegisteredStreamEvents<I> {
    global().register_stream(Some(&handle), owner, close);
    RegisteredStreamEvents {
        inner: events,
        handle,
    }
}

/// 资源使用期间的注册守卫；离开作用域即注销。
#[must_use]
pub struct RegisteredResource {
    handle: Arc<CancelHandle>,
}

impl RegisteredResource {
    pub fn handle(&self) -> &Arc<CancelHandle> {
        &self.handle
    }
}

impl Drop for RegisteredResource {
    fn drop(&mut self) {
        global().unregister_resource(&self.handle);
    }
}

/// 在资源使用期间注册其取消句柄，并在守卫丢弃时注销。
pub fn registered_resource(
    handle: Arc<CancelHandle>,
    owner: Option<ScopeOwner>,
    close: Option<CloseAction>,
) -> RegisteredResource {
    global().register_resource(Some(&handle), owner, close);
    RegisteredResource { handle }
}

fn register_in(
    collection: &Mutex<Vec<Entry>>,
    handle: Option<&Arc<CancelHandle>>,
    owner: Option<ScopeOwner>,
    close: Option<CloseAction>,
) {
    let Some(handle) = handle else {
        return;
    };
    let owner = owner.or_else(current_stream_scope);
    let mut entries = collection.lock().expect("注册表锁");
    if entries
        .iter()
        .any(|entry| Arc::ptr_eq(&entry.handle, handle))
    {
        return;
    }
    entries.push(Entry {
        handle: Arc::clone(handle),
        owner,
        close,
    });
}

fn unregister_in(collection: &Mutex<Vec<Entry>>, handle: &Arc<CancelHandle>) {
    let mut entries = collection.lock().expect("注册表锁");
    entries.retain(|entry| !Arc::ptr_eq(&entry.handle, handle));
}

fn close_active_in(collection: &Mutex<Vec<Entry>>, owner: Option<&ScopeOwner>) -> usize {
    let targets: Vec<Entry> = {
        let entries = collection.lock().expect("注册表锁");
        entries
            .iter()
            .filter(|entry| match owner {
                None => true,
                Some(owner) => entry
                    .owner
                    .as_ref()
                    .is_some_and(|entry_owner| entry_owner.same(owner)),
            })
            .cloned()
            .collect()
    };

    let mut closed = 0usize;
    for entry in targets {
        let effective = entry.close.clone();
        match effective {
            Some(close) => {
                close();
                closed += 1;
            }
            None if entry.handle.has_close() => {
                entry.handle.invoke_close();
                closed += 1;
            }
            None => {}
        }
        unregister_in(collection, &entry.handle);
    }
    closed
}

fn count_active_in(collection: &Mutex<Vec<Entry>>, owner: Option<&ScopeOwner>) -> usize {
    let entries = collection.lock().expect("注册表锁");
    match owner {
        None => entries.len(),
        Some(owner) => entries
            .iter()
            .filter(|entry| {
                entry
                    .owner
                    .as_ref()
                    .is_some_and(|entry_owner| entry_owner.same(owner))
            })
            .count(),
    }
}
