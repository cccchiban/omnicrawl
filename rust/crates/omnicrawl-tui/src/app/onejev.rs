//! OneJev 运行环境/下载/服务，以及本地决策服务的自启动。
//!
//! 这一块与 `App` 的界面状态几乎不相交：装配的是外部进程与后台任务，进度经通道回填。
//! 因此单独成文件，避免把「外部服务编排」和「界面对话」混在一处。

use super::*;

impl App {
    /// 一键准备运行环境（venv + torch + qev）：长任务，放后台线程。
    pub(super) fn start_onejev_environment(&mut self) -> Result<String, String> {
        self.spawn_onejev_task(
            "准备运行环境",
            |environment, configuration, progress| {
                let root = resolve_onejev_root(&environment, &configuration.model_dir);
                let mut sink = progress.sink();
                let result = ensure_onejev_environment(&environment, &root, &mut sink)
                    .map(|outcome| outcome.cuda);
                OneJevTaskResult::Environment(result)
            },
        )
    }

    /// 删除运行环境（venv），保留权重与下载缓存。
    ///
    /// 「准备运行环境」按本机 GPU 能力选 torch 轮子；驱动当时不可用就会装成 CPU 版，
    /// 只能删掉重装。服务在跑时先停掉：Windows 上被占用的文件删不掉。
    pub(super) fn remove_onejev_environment(&mut self) -> Result<String, String> {
        if self.onejev_task.is_some() {
            return Err("已有自部署任务在跑，请稍候。".to_string());
        }
        let environment = ConfigEnvironment::from_process();
        let configuration = load_local_deployment(&environment, None);
        let root = resolve_onejev_root(&environment, &configuration.model_dir);
        let venv = omnicrawl_onejev::paths::venv_dir(&root);
        if !venv.exists() {
            return Ok("运行环境本来就没装，无需删除。".to_string());
        }
        // 删 venv 会连同 qev 一起删掉：服务必须真的停掉，否则 Windows 上文件删不掉。
        self.onejev_server.stop_shared(&root);
        let removed = remove_onejev_env(&root)?;
        self.refresh_decision_local();
        if !removed {
            return Ok("运行环境本来就没装，无需删除。".to_string());
        }
        Ok(format!(
            "已删除运行环境（{}），权重与下载缓存保留；可按「准备运行环境」重装。",
            venv.display()
        ))
    }

    /// 下载某个尺寸的权重：GB 级长任务，放后台线程。
    pub(super) fn start_onejev_download(&mut self, size: &str) -> Result<String, String> {
        let Some(target) = find_onejev_size(size) else {
            return Err(format!("未知的自部署尺寸：{size}。"));
        };
        let name = format!("下载 OneJev {}", target.key);
        self.spawn_onejev_task(&name, move |environment, configuration, progress| {
            let root = resolve_onejev_root(&environment, &configuration.model_dir);
            progress.stage(format!("下载 OneJev {}", target.key));
            let mut sink = progress.bytes_sink();
            let result =
                download_onejev_model(&environment, &root, &target, Some(&mut sink)).map(|_| ());
            OneJevTaskResult::Download {
                size: target.key.to_string(),
                result,
            }
        })
    }

    /// 删除某个尺寸的权重（同步动作：只有目录删除，无网络）。
    pub(super) fn delete_onejev_size(&mut self, size: &str) -> Result<String, String> {
        let Some(target) = find_onejev_size(size) else {
            return Err(format!("未知的自部署尺寸：{size}。"));
        };
        let environment = ConfigEnvironment::from_process();
        let configuration = load_local_deployment(&environment, None);
        let root = resolve_onejev_root(&environment, &configuration.model_dir);
        // 删的是服务正在用的那份权重时先停服务：Windows 上被占用的文件删不掉。
        // 服务可能是别的实例起的，因此判定要看「在跑的是哪一档」，不只看本实例的记录。
        let target_dir = onejev_model_dir(&root, target.repo_id);
        let target_model = target.repo_id.rsplit('/').next().unwrap_or(target.key);
        let in_use = self.onejev_server.model_dir() == Some(target_dir.as_path())
            || omnicrawl_onejev::running_model(&root).as_deref() == Some(target_model);
        if in_use {
            self.onejev_server.stop_shared(&root);
        }
        let removed = delete_onejev_model(&root, &target)?;
        if !removed {
            return Ok(format!("OneJev {} 未下载到本机，无需删除。", target.key));
        }
        self.refresh_decision_local();
        Ok(format!("已删除 OneJev {} 的权重。", target.key))
    }

    /// 把决策渠道切到自部署：建/改一条 `onejev` 渠道并设为默认。
    ///
    /// 尺寸决定模型名（worker 的 `--name`），传输走本机管道；服务没跑就顺手拉起。
    pub(super) fn use_onejev_channel(&mut self, size: &str) -> Result<String, String> {
        let Some(target) = find_onejev_size(size) else {
            return Err(format!("未知的自部署尺寸：{size}。"));
        };
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_decision_model_configuration(&environment, None)
            .unwrap_or_else(|_| DecisionModelConfiguration::default());
        let model_name = target
            .repo_id
            .rsplit('/')
            .next()
            .unwrap_or(target.key)
            .to_string();
        let channel = DecisionChannelConfig {
            key: DEFAULT_LOCAL_DECISION_CHANNEL_KEY.to_string(),
            name: format!("OneJev 自部署 {}", target.key),
            mode: DECISION_MODE_ONEJEV.to_string(),
            base_url: DEFAULT_LOCAL_DECISION_BASE_URL.to_string(),
            api_key: String::new(),
            model: model_name,
            retry_count: DEFAULT_DECISION_RETRY_COUNT,
            enabled: true,
        };
        match configuration
            .channels
            .iter_mut()
            .find(|item| item.key == DEFAULT_LOCAL_DECISION_CHANNEL_KEY)
        {
            Some(existing) => *existing = channel,
            None => configuration.channels.push(channel),
        }
        configuration.default_key = DEFAULT_LOCAL_DECISION_CHANNEL_KEY.to_string();
        let path = save_decision_model_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        self.rebuild_review(&environment);
        self.rebuild_custody(&environment);
        self.rebuild_suggest(&environment);
        self.rebuild_rerank(&environment)?;
        let rows: Vec<DecisionRow> = configuration
            .channels
            .iter()
            .map(decision_row_from_config)
            .collect();
        if let Some(settings) = self.settings.as_mut() {
            settings.sync_decision_models(rows, &configuration.default_key);
        }

        // 渠道指向本地服务而服务没跑：顺手拉起，免得用户切完渠道却打不通。
        // 本机已有服务在跑（本实例或别的实例起的）时只是复用，不会重复拉起；
        // `[local] auto_start = false` 时只切渠道、不动进程（启动与否交给「启动服务」行）。
        let local = load_local_deployment(&environment, None);
        let note = if local.auto_start {
            match self.ensure_onejev_service(&local, false) {
                Ok(message) => format!(" {message}"),
                Err(error) => format!(" 本地服务未启动：{error}"),
            }
        } else {
            " [local] auto_start = false，未自动启动本地服务。".to_string()
        };
        Ok(format!(
            "决策渠道已切到自部署（{}，走本机管道），已保存到 {}{}。",
            target.key,
            path.display(),
            note
        ))
    }

    /// 确保本地服务可用：端口上已有就复用，否则按当前规格拉起。
    pub(super) fn ensure_onejev_service(
        &mut self,
        configuration: &LocalDeploymentConfig,
        force_restart: bool,
    ) -> Result<String, String> {
        if self.onejev_task.is_some() {
            return Err("已有自部署任务在跑，请稍候。".to_string());
        }
        let Some(target) = find_onejev_size(&configuration.size) else {
            return Err(format!("未知的自部署尺寸：{}。", configuration.size));
        };
        let environment = ConfigEnvironment::from_process();
        let root = resolve_onejev_root(&environment, &configuration.model_dir);
        let spec = OneJevLaunchSpec::new(&root, &target, &configuration.device);
        let timeout = configuration.health_timeout_seconds.max(1) as u64;
        let (sender, receiver) = mpsc::channel();
        self.onejev_task = Some(receiver);
        let progress = omnicrawl_onejev::ProgressHandle::new();
        let task_name = if force_restart {
            format!("重启本地决策服务（{}）", target.key)
        } else {
            format!("启动本地决策服务（{}）", target.key)
        };
        progress.stage("正在拉起 qev 进程…");
        self.onejev_progress = Some(OneJevRunningTask {
            name: task_name.clone(),
            progress: progress.clone(),
        });
        // 服务句柄先搬到线程里等健康检查，结果连同句柄一起送回来装回宿主。
        let mut server = std::mem::take(&mut self.onejev_server);
        let status = format!("正在启动本地决策服务（{}）…", target.key);
        thread::spawn(move || {
            let mut on_stage = |stage: &str| progress.stage(stage);
            let outcome = if force_restart {
                server.restart(spec, timeout)
            } else {
                server.start_with_progress(spec, true, timeout, &mut on_stage)
            };
            let result = match outcome.state {
                OneJevServerState::Ready => Ok(outcome.message),
                _ => Err(outcome.message),
            };
            let _ = sender.send(OneJevTaskResult::Service { server, result });
        });
        self.refresh_decision_local_with(status.clone());
        Ok(status)
    }

    /// 启动（或复用）本地服务；规格变了时由调用方走 [`Self::ensure_onejev_service`] 的强制重启。
    pub(super) fn start_onejev_service(&mut self) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let configuration = load_local_deployment(&environment, None);
        self.ensure_onejev_service(&configuration, false)
    }

    /// 按新规格重启本地服务（尺寸/设备改档时用）：这份服务本机只有一份，必须换进程。
    pub(super) fn restart_onejev_service(
        &mut self,
        configuration: &LocalDeploymentConfig,
    ) -> Result<String, String> {
        self.ensure_onejev_service(configuration, true)
    }

    /// 停止本机共享的本地服务并释放显存。
    ///
    /// 停的是端口上那一份（可能是别的实例起的）：多实例共用一份服务，界面上的「停止服务」
    /// 就是要它真的停下来。
    pub(super) fn stop_onejev_service(&mut self) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let configuration = load_local_deployment(&environment, None);
        let root = resolve_onejev_root(&environment, &configuration.model_dir);
        let outcome = self.onejev_server.stop_shared(&root);
        self.refresh_decision_local();
        Ok(outcome.message)
    }

    /// 把「读配置 + 后台执行」的样板收口：任务结果统一回到 [`Self::tick_onejev_tasks`]。
    ///
    /// `name` 是界面上显示的任务名；`run` 通过 [`omnicrawl_onejev::ProgressHandle`] 持续写进度，
    /// 界面每帧取快照。任务结束时进度句柄随 `onejev_progress` 一起清空，`busy` 因此必然复位。
    pub(super) fn spawn_onejev_task<F>(&mut self, name: &str, run: F) -> Result<String, String>
    where
        F: FnOnce(
                ConfigEnvironment,
                LocalDeploymentConfig,
                omnicrawl_onejev::ProgressHandle,
            ) -> OneJevTaskResult
            + Send
            + 'static,
    {
        if self.onejev_task.is_some() {
            return Err("已有自部署任务在跑，请稍候。".to_string());
        }
        let environment = ConfigEnvironment::from_process();
        let configuration = load_local_deployment(&environment, None);
        let (sender, receiver) = mpsc::channel();
        self.onejev_task = Some(receiver);
        let progress = omnicrawl_onejev::ProgressHandle::new();
        self.onejev_progress = Some(OneJevRunningTask {
            name: name.to_string(),
            progress: progress.clone(),
        });
        thread::spawn(move || {
            let _ = sender.send(run(environment, configuration, progress));
        });
        self.refresh_decision_local_with(format!("{name}已开始。"));
        Ok(format!("{name}已开始，进度见「任务进度」行。"))
    }

    /// 每帧轮询后台自部署任务：取回结果后回填状态行并刷新分区。
    ///
    /// 任务在跑时每帧把进度快照推给设置页（进度行与状态行都读它）；结果一到就清空
    /// [`Self::onejev_progress`]——`busy` 由它推导，因此**必然**在任务结束时复位。
    pub fn tick_onejev_tasks(&mut self) {
        // 先推这一帧的进度：任务结束的那一帧也要把最终阶段画出来。
        if self.onejev_progress.is_some() && self.onejev_task.is_some() {
            self.refresh_decision_local();
        }
        let received = match self.onejev_task.as_ref() {
            Some(receiver) => receiver.try_recv(),
            None => return,
        };
        let outcome = match received {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.onejev_task = None;
                self.onejev_progress = None;
                self.refresh_decision_local();
                return;
            }
        };
        self.onejev_task = None;
        self.onejev_progress = None;
        let message = match outcome {
            OneJevTaskResult::Environment(Ok(true)) => {
                "运行环境已就绪（CUDA 版 torch）。".to_string()
            }
            OneJevTaskResult::Environment(Ok(false)) => {
                "运行环境已就绪（CPU 版 torch：未检测到可用的 NVIDIA 驱动）。".to_string()
            }
            OneJevTaskResult::Environment(Err(error)) => format!("运行环境准备失败：{error}"),
            OneJevTaskResult::Download { size, result } => match result {
                Ok(()) => format!("OneJev {size} 下载完成。"),
                Err(error) => format!("OneJev {size} 下载失败：{error}"),
            },
            OneJevTaskResult::Service { server, result } => {
                self.onejev_server = server;
                match result {
                    Ok(message) => message,
                    Err(error) => format!("本地决策服务未就绪：{error}"),
                }
            }
        };
        self.refresh_decision_local_with(message);
    }

    /// 用磁盘与进程的现状刷新自部署分区（保留既有状态行文本）。
    pub(super) fn refresh_decision_local(&mut self) {
        self.refresh_decision_local_with(String::new());
    }

    /// 刷新自部署分区；`message` 非空时同时更新状态行。
    pub(super) fn refresh_decision_local_with(&mut self, message: String) {
        let environment = ConfigEnvironment::from_process();
        let values = self.decision_local_values(&environment);
        if let Some(settings) = self.settings.as_mut() {
            settings.refresh_decision_local(values, message);
        }
    }

    // ---------- 启动期自启动本地决策服务 ----------

    /// 启动时按配置把决策 REST 接口准备好（`decision_models.toml` 的 `[api]` 段）。
    ///
    /// 端口已就绪（含**其他 OmniCrawl 实例**起的服务）就复用，不重复拉起；未启用或没配
    /// 令牌时不动任何进程——它是可选的外部调用入口，不在启动期起一个注定拒绝监听的进程。
    pub fn autostart_decision_api(&mut self) {
        let environment = ConfigEnvironment::from_process();
        let settings = omnicrawl_decision::load_settings(&environment);
        if let Some(reason) = settings.unavailable_reason() {
            // 未启用是常态（默认关），只有配了一半（启用了但缺令牌/渠道）才值得提示。
            if settings.api.enabled {
                self.startup_service_lines
                    .push(format!("决策接口未启动：{reason}"));
            }
            return;
        }
        let outcome = omnicrawl_decision::ensure_running(&environment, &settings, false);
        self.startup_service_lines.push(outcome.message);
    }

    /// 启动时按配置把自部署决策服务准备好（渠道 `onejev` + `[local] auto_start`）。
    ///
    /// 判定与处理都集中在 `OneJevServer`：端口已就绪（含**其他 OmniCrawl 实例**起的服务）
    /// 就复用，不重复拉起；环境与权重不齐时只记一行启动日志，不在启动期起一个注定失败的进程。
    pub fn autostart_local_decision_service(&mut self) {
        let environment = ConfigEnvironment::from_process();
        let Ok(configuration) = load_decision_model_configuration(&environment, None) else {
            return;
        };
        let Some(channel) = configuration.active_channel() else {
            return;
        };
        // 默认渠道不是本机自部署：本实例不用本地服务，不动任何进程。
        // 云端网关也走 `onejev` 协议形状，因此 `is_local_service` 会同时确认地址落在回环上。
        if !channel.is_local_service() {
            return;
        }
        let local = load_local_deployment(&environment, None);
        if !local.auto_start {
            self.startup_service_lines.push(
                "默认决策渠道是自部署，但 [local] auto_start = false，未自动启动本地服务。"
                    .to_string(),
            );
            return;
        }
        let Some(target) = find_onejev_size(&local.size) else {
            return;
        };
        let root = resolve_onejev_root(&environment, &local.model_dir);
        let spec = OneJevLaunchSpec::new(&root, &target, &local.device);

        // 本机已经有就绪的服务：复用即可，这是多实例共用的正常路径。
        // 这里同步复用（不拉起进程、也不占后台任务），只记一行启动日志。
        if omnicrawl_onejev::service_ready(&root) {
            let outcome = self
                .onejev_server
                .start_with_progress(spec, false, 1, &mut |_| {});
            self.startup_service_lines.push(outcome.message);
            return;
        }

        // 环境与权重是启动的前提：缺了就让用户去设置页处理，不在这里起一个注定失败的进程。
        if onejev_environment_state(&root) != omnicrawl_onejev::EnvironmentState::Ready {
            self.startup_service_lines.push(
                "自部署运行环境未就绪，未自动启动本地服务（可在「结构化决策模型」页准备运行环境）。"
                    .to_string(),
            );
            return;
        }
        if !onejev_model_ready(&root, &target) {
            self.startup_service_lines.push(format!(
                "OneJev {} 的权重未下载，未自动启动本地服务（可在「结构化决策模型」页下载）。",
                target.key
            ));
            return;
        }

        // 启动是分钟级动作（加载权重 + 捕获 CUDA 图）：放后台线程，启动画面不等它。
        match self.ensure_onejev_service(&local, false) {
            Ok(message) => self.startup_service_lines.push(message),
            Err(error) => self
                .startup_service_lines
                .push(format!("本地决策服务未启动：{error}")),
        }
    }

}
