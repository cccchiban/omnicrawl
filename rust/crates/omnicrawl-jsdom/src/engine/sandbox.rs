//! 沙箱的一次执行：建上下文、装全局、跑脚本、驱动微任务。
//!
//! 从 `engine.rs` 拆出。这里是「一次 run 从头到尾」的编排，读它就能看清
//! 求值顺序；具体实现分散在父模块的 `eval_*` / `install_*` / `drive_*` 里。

use super::*;

pub(crate) fn run_in_process(
    html: &str,
    profile: &Profile,
    loader: Option<&(dyn ScriptSource + '_)>,
    http: Option<&(dyn HttpSource + '_)>,
) -> RunOutcome {
    let collected = collect_scripts(html, profile.href.as_str());
    let truncated = collected.len() > MAX_INLINE_SCRIPTS;
    let collected: Vec<ScriptRef> = collected.into_iter().take(MAX_INLINE_SCRIPTS).collect();

    SESSION.with(|slot| {
        *slot.borrow_mut() = Some(Session {
            dom: Dom::from_html(html, MAX_DOM_NODES),
            written: String::new(),
            touched: false,
            errors: Vec::new(),
        });
    });
    // 节点对象缓存按执行隔离：上一份文档的节点号在这次执行里会指向别的节点。
    clear_element_cache();
    // Canvas 位图同样按执行隔离：位图键是元素号，跨执行复用会让新文档读到旧像素。
    crate::browser_env::clear_canvas_state();
    // 通道在这里登记、在本函数末尾清空：中间所有脚本都能用到它。
    fetch_api::install(http, profile.href.as_str());

    // 模块解析钉死在 Idle：默认构建会尝试用 `SimpleModuleLoader` 按进程工作目录
    // 解析说明符，从而读真实文件系统；沙箱只认已声明的脚本，不认本地模块图。
    let mut context = match Context::builder()
        .module_loader(Rc::new(IdleModuleLoader))
        .build()
    {
        Ok(context) => context,
        Err(failure) => {
            let message = one_line(&failure.to_string());
            with_session(|session| session.errors.push(format!("沙箱初始化失败：{message}")));
            return finish(0, truncated);
        }
    };
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(LOOP_ITERATION_LIMIT);

    let mut executed = 0usize;
    match install_globals(&mut context, profile) {
        Ok(()) => {
            // 先跑普通脚本，再跑 defer：两者各自保持文档顺序。
            for deferred in [false, true] {
                for script in collected.iter().filter(|script| script.defer == deferred) {
                    let body = match &script.kind {
                        ScriptKind::Inline => script.body.clone(),
                        ScriptKind::External => match loader {
                            Some(loader) => match loader.load(&script.body) {
                                Ok(body) if body.len() <= MAX_SCRIPT_BYTES => body,
                                Ok(_) => {
                                    with_session(|session| {
                                        session.errors.push(format!(
                                            "外链脚本超出体积上限，已跳过：{}",
                                            script.body
                                        ))
                                    });
                                    continue;
                                }
                                Err(reason) => {
                                    with_session(|session| {
                                        session.errors.push(format!(
                                            "外链脚本取数失败（{}）：{}",
                                            reason, script.body
                                        ))
                                    });
                                    continue;
                                }
                            },
                            // 没给加载器：与旧行为一致，跳过但不报错。
                            None => continue,
                        },
                    };
                    executed += 1;
                    // 给脚本带上来源路径：boa 的报错与堆栈会因此带文件名，
                    // 否则一份页面里多个脚本只能靠位置猜。
                    let label = match &script.kind {
                        ScriptKind::Inline => format!("inline#{executed}"),
                        ScriptKind::External => script.body.clone(),
                    };
                    let source = Source::from_bytes(&body).with_path(std::path::Path::new(&label));
                    let outcome = if script.module {
                        eval_module(&mut context, source)
                    } else {
                        context.eval(source).map(|_| ())
                    };
                    if let Err(failure) = outcome {
                        with_session(|session| {
                            session.errors.push(format!(
                                "{label}：{}",
                                script_error_detail(&failure.to_string())
                            ))
                        });
                    }
                    // 每个脚本跑完就把微任务推进一步：SPA 常在脚本里 `fetch(...).then(渲染)`，
                    // 只靠最后统一 flush 会让后面的脚本看到还没渲染的 DOM。
                    drive_microtasks(&mut context);
                }
            }
            // 页面常把主逻辑放在 `setTimeout(fn, 0)` 里（指纹脚本就是）。
            // 这里不睡眠，只把已登记的回调跑几轮，再收正文；
            // 每一轮之后都推一次微任务，因为回调里往往又发了一次 `fetch`。
            for _ in 0..MAX_TIMER_ROUNDS {
                if !browser_env::flush_timers_once(&mut context) {
                    break;
                }
                drive_microtasks(&mut context);
            }
        }
        Err(failure) => {
            let message = one_line(&failure.to_string());
            with_session(|session| session.errors.push(format!("沙箱初始化失败：{message}")));
        }
    }

    finish(executed, truncated)
}
