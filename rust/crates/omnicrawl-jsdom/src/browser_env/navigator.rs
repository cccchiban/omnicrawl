//! 由 `browser_env.rs` 按假装的全局对象拆出，语义未变。

use super::*;







pub(crate) fn install_navigator(context: &mut Context, profile: &Profile) -> JsResult<()> {
    let languages = JsArray::new(context)?;
    for language in profile.languages() {
        languages.push(JsString::from(language), context)?;
    }

    let mime_types = build_mime_types(context)?;
    let plugins = build_plugins(context)?;

    // Navigator 的字段在真机上是原型上的**只读访问器**：`navigator.userAgent = 'x'`
    // 静默失败，`Object.getOwnPropertyDescriptor` 拿到 `{get: f, set: undefined}`。
    // 用可写数据属性会把「这是伪装」直接暴露给描述符探测。
    let values: Vec<(&str, JsValue)> = vec![
        ("userAgent", JsString::from(profile.user_agent.clone()).into()),
        ("appVersion", JsString::from(profile.app_version.clone()).into()),
        ("appName", js_string!("Netscape").into()),
        ("appCodeName", js_string!("Mozilla").into()),
        ("product", js_string!("Gecko").into()),
        ("productSub", js_string!("20030107").into()),
        ("vendor", JsString::from(profile.vendor).into()),
        ("vendorSub", js_string!("").into()),
        ("platform", JsString::from(profile.platform).into()),
        ("language", JsString::from(profile.locale).into()),
        ("languages", languages.into()),
        ("onLine", JsValue::from(true)),
        ("cookieEnabled", JsValue::from(true)),
        ("doNotTrack", JsValue::null()),
        (
            "hardwareConcurrency",
            JsValue::from(profile.hardware_concurrency),
        ),
        ("deviceMemory", JsValue::from(profile.device_memory)),
        ("maxTouchPoints", JsValue::from(0)),
        // 自动化痕迹：真实有头浏览器为 false。
        ("webdriver", JsValue::from(false)),
        ("pdfViewerEnabled", JsValue::from(true)),
        ("plugins", plugins),
        ("mimeTypes", mime_types),
    ];
    let navigator = ObjectInitializer::new(context).build();
    for (name, value) in values {
        // 值放进 `__ornSlot..` 字段，访问器读它：取值照常，赋值被忽略。
        let slot = format!("__ornSlot:{name}");
        let _ = navigator.set(JsString::from(slot), value, false, context);
        define_slot_accessor(context, &navigator, name);
    }

    if profile.client_hints {
        let brands = JsArray::new(context)?;
        for (name, version) in profile.brands() {
            let brand = ObjectInitializer::new(context)
                .property(js_string!("brand"), JsString::from(name), Attribute::all())
                .property(
                    js_string!("version"),
                    JsString::from(version),
                    Attribute::all(),
                )
                .build();
            brands.push(brand, context)?;
        }
        // 嵌套初始化器必须先在独立语句里建好：`ObjectInitializer` 持有 `&mut Context`。
        let high_entropy = ObjectInitializer::new(context)
            .property(
                js_string!("platformVersion"),
                js_string!(profile.platform_version),
                Attribute::all(),
            )
            .property(
                js_string!("uaFullVersion"),
                js_string!(profile.full_version),
                Attribute::all(),
            )
            .build();
        let ua_data = ObjectInitializer::new(context)
            .property(js_string!("brands"), brands, Attribute::all())
            .property(js_string!("mobile"), JsValue::from(false), Attribute::all())
            .property(
                js_string!("platform"),
                js_string!(profile.ch_platform),
                Attribute::all(),
            )
            .function(
                native(uad_high_entropy),
                js_string!("getHighEntropyValues"),
                1,
            )
            .function(native(uad_to_json), js_string!("toJSON"), 0)
            .property(
                js_string!("__ornHighEntropy"),
                high_entropy,
                Attribute::READONLY,
            )
            .build();
        navigator.set(js_string!("userAgentData"), ua_data, false, context)?;
    }

    // 这两个是 `navigator` 的属性而非全局：探测器都从 `navigator.permissions` 读。
    let permissions = ObjectInitializer::new(context)
        .function(native(perm_query), js_string!("query"), 1)
        .build();
    navigator.set(js_string!("permissions"), permissions, false, context)?;
    let connection = ObjectInitializer::new(context)
        .property(js_string!("downlink"), JsValue::from(10), Attribute::all())
        .property(
            js_string!("effectiveType"),
            js_string!("4g"),
            Attribute::all(),
        )
        .property(js_string!("rtt"), JsValue::from(50), Attribute::all())
        .property(
            js_string!("saveData"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(js_string!("onchange"), JsValue::null(), Attribute::all())
        .build();
    navigator.set(js_string!("connection"), connection, false, context)?;
    let _ = context.register_global_property(js_string!("navigator"), navigator, Attribute::all());
    Ok(())
}

/// `navigator.userAgentData.getHighEntropyValues`：只回填调用方请求的字段。
pub(crate) fn uad_high_entropy(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let requested: Vec<String> = args
        .first()
        .and_then(|value| value.as_object())
        .map(|array| {
            let length = array
                .get(js_string!("length"), context)
                .ok()
                .and_then(|value| value.to_number(context).ok())
                .unwrap_or(0.0) as usize;
            (0..length)
                .filter_map(|index| {
                    array
                        .get(index, context)
                        .ok()
                        .and_then(|value| value.to_string(context).ok())
                        .map(|text| text.to_std_string_escaped())
                })
                .collect()
        })
        .unwrap_or_default();

    // 先在独立语句里把要读的值取出来：`ObjectInitializer` 会独占 `&mut Context`，
    // 建好之后再调 `object.get(.., context)` 就借不到 context 了。
    let brands_value = object.get(js_string!("brands"), context)?;
    let platform_value = object.get(js_string!("platform"), context)?;
    let high_entropy = object
        .get(js_string!("__ornHighEntropy"), context)
        .ok()
        .and_then(|value| value.as_object());
    let platform_version_value = high_entropy
        .as_ref()
        .and_then(|source| source.get(js_string!("platformVersion"), context).ok())
        .unwrap_or(JsValue::undefined());
    let ua_full_version_value = high_entropy
        .as_ref()
        .and_then(|source| source.get(js_string!("uaFullVersion"), context).ok())
        .unwrap_or(JsValue::undefined());

    let mut result = ObjectInitializer::new(context);
    result.property(js_string!("brands"), brands_value.clone(), Attribute::all());
    result.property(js_string!("mobile"), JsValue::from(false), Attribute::all());
    result.property(js_string!("platform"), platform_value, Attribute::all());
    if requested.iter().any(|name| name == "architecture") {
        result.property(
            js_string!("architecture"),
            js_string!("x86"),
            Attribute::all(),
        );
    }
    if requested.iter().any(|name| name == "bitness") {
        result.property(js_string!("bitness"), js_string!("64"), Attribute::all());
    }
    if requested.iter().any(|name| name == "model") {
        result.property(js_string!("model"), js_string!(""), Attribute::all());
    }
    if requested.iter().any(|name| name == "platformVersion") {
        result.property(
            js_string!("platformVersion"),
            platform_version_value,
            Attribute::all(),
        );
    }
    if requested.iter().any(|name| name == "uaFullVersion") {
        result.property(
            js_string!("uaFullVersion"),
            ua_full_version_value,
            Attribute::all(),
        );
    }
    if requested.iter().any(|name| name == "fullVersionList") {
        result.property(
            js_string!("fullVersionList"),
            brands_value,
            Attribute::all(),
        );
    }
    if requested.iter().any(|name| name == "wow64") {
        result.property(js_string!("wow64"), JsValue::from(false), Attribute::all());
    }
    let resolved = result.build();
    Ok(JsPromise::resolve(resolved, context)?.into())
}

pub(crate) fn uad_to_json(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let brands = object.get(js_string!("brands"), context)?;
    let platform = object.get(js_string!("platform"), context)?;
    let shape = ObjectInitializer::new(context)
        .property(js_string!("brands"), brands, Attribute::all())
        .property(js_string!("mobile"), JsValue::from(false), Attribute::all())
        .property(js_string!("platform"), platform, Attribute::all())
        .build();
    Ok(shape.into())
}

/// 真实 Chrome 的 `navigator.mimeTypes` 是两条 `application/pdf`，
/// `plugins` 是五项同源 PDF 插件（Chromium 系固定形状）。
pub(crate) fn build_mime_types(context: &mut Context) -> JsResult<JsValue> {
    let entry = ObjectInitializer::new(context)
        .property(
            js_string!("type"),
            js_string!("application/pdf"),
            Attribute::all(),
        )
        .property(js_string!("suffixes"), js_string!("pdf"), Attribute::all())
        .property(
            js_string!("description"),
            js_string!("Portable Document Format"),
            Attribute::all(),
        )
        .build();
    let array = JsArray::new(context)?;
    let _ = array.push(entry.clone(), context);
    let _ = array.push(entry.clone(), context);
    // 真机里两条条目都是 `application/pdf`，同时按名字可索引（名字索引不占 length）。
    let _ = array.set(js_string!("application/pdf"), entry, false, context);
    Ok(array.into())
}

pub(crate) fn build_plugins(context: &mut Context) -> JsResult<JsValue> {
    let names = [
        "PDF Viewer",
        "Chrome PDF Viewer",
        "Chromium PDF Viewer",
        "Microsoft Edge PDF Viewer",
        "WebKit built-in PDF",
    ];
    let array = JsArray::new(context)?;
    for name in names {
        let entry = ObjectInitializer::new(context)
            .property(js_string!("name"), js_string!(name), Attribute::all())
            .property(
                js_string!("filename"),
                js_string!("internal-pdf-viewer"),
                Attribute::all(),
            )
            .property(
                js_string!("description"),
                js_string!("Portable Document Format"),
                Attribute::all(),
            )
            .property(js_string!("length"), JsValue::from(1), Attribute::all())
            .build();
        let _ = array.push(entry, context);
    }
    Ok(array.into())
}

pub(crate) fn install_screen(context: &mut Context, profile: &Profile) {
    let orientation = ObjectInitializer::new(context)
        .property(
            js_string!("type"),
            js_string!("landscape-primary"),
            Attribute::all(),
        )
        .property(js_string!("angle"), JsValue::from(0), Attribute::all())
        .build();
    // `Screen` 的每个字段在真机上都是**只读访问器**：`screen.width = 999` 静默失败，
    // 而 `Object.getOwnPropertyDescriptor` 拿到的是 `{get: f, set: undefined}`。
    // 用可写数据属性会把「伪装」这件事直接暴露给描述符探测。
    let realm = context.realm().clone();
    let getter = |pointer: boa_engine::native_function::NativeFunctionPointer| {
        Some(NativeFunction::from_fn_ptr(pointer).to_js_function(&realm))
    };
    let dimension = |value: u32| -> JsValue { JsValue::from(value) };
    // 各字段的取值通过捕获固定的闭包给出：每个 getter 需要自己的常量。
    let screen = ObjectInitializer::new(context)
        .accessor(
            js_string!("width"),
            getter(screen_width),
            None,
            Attribute::CONFIGURABLE,
        )
        .accessor(
            js_string!("height"),
            getter(screen_height),
            None,
            Attribute::CONFIGURABLE,
        )
        .accessor(
            js_string!("availWidth"),
            getter(screen_avail_width),
            None,
            Attribute::CONFIGURABLE,
        )
        .accessor(
            js_string!("availHeight"),
            getter(screen_avail_height),
            None,
            Attribute::CONFIGURABLE,
        )
        .accessor(
            js_string!("colorDepth"),
            getter(screen_color_depth),
            None,
            Attribute::CONFIGURABLE,
        )
        .accessor(
            js_string!("pixelDepth"),
            getter(screen_pixel_depth),
            None,
            Attribute::CONFIGURABLE,
        )
        .accessor(
            js_string!("availLeft"),
            getter(screen_zero),
            None,
            Attribute::CONFIGURABLE,
        )
        .accessor(
            js_string!("availTop"),
            getter(screen_zero),
            None,
            Attribute::CONFIGURABLE,
        )
        .build();
    let _ = screen.set(js_string!("orientation"), orientation, false, context);
    // getter 从 `screen` 对象自身读不到画像，因此画像数值挂在隐藏字段上。
    for (name, value) in [
        ("__ornWidth", dimension(profile.width)),
        ("__ornHeight", dimension(profile.height)),
        (
            "__ornAvailHeight",
            dimension(profile.height.saturating_sub(40)),
        ),
        ("__ornColorDepth", dimension(24)),
    ] {
        let _ = screen.set(js_string!(name), value, false, context);
    }
    let _ = context.register_global_property(js_string!("screen"), screen, Attribute::all());
}

/// Screen 的 getter 们：`this` 指向 `screen`，取值来自它的隐藏字段。
fn screen_read(this: &JsValue, key: &str, context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    Ok(object
        .get(JsString::from(key), context)
        .unwrap_or(JsValue::undefined()))
}

fn screen_width(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    screen_read(this, "__ornWidth", context)
}

fn screen_height(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    screen_read(this, "__ornHeight", context)
}

fn screen_avail_width(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    screen_read(this, "__ornWidth", context)
}

fn screen_avail_height(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    screen_read(this, "__ornAvailHeight", context)
}

fn screen_color_depth(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    screen_read(this, "__ornColorDepth", context)
}

fn screen_pixel_depth(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    screen_read(this, "__ornColorDepth", context)
}

fn screen_zero(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(0))
}

pub(crate) fn install_window_metrics(context: &mut Context, profile: &Profile) {
    // 视口小于窗口：浏览器界面（标签栏+地址栏）占掉的那部分在真实环境里必然存在。
    let metrics = [
        ("innerWidth", profile.width as i64),
        ("innerHeight", profile.height as i64 - 128),
        ("outerWidth", profile.width as i64),
        ("outerHeight", profile.height as i64),
        ("screenX", 0),
        ("screenY", 0),
        ("screenLeft", 0),
        ("screenTop", 0),
        ("scrollX", 0),
        ("scrollY", 0),
        ("pageXOffset", 0),
        ("pageYOffset", 0),
    ];
    for (name, value) in metrics {
        // boa 已内置部分同名属性（如 `innerHeight`），`register_global_property` 对已存在
        // 且不可配置的属性无效；这里显式覆盖描述符，读到的才是伪装值。
        let descriptor = || {
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(value))
                .writable(false)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = context.global_object().define_property_or_throw(
            JsString::from(name),
            descriptor(),
            context,
        );
        // `window` 是单独创建的对象（持 `document` 访问器），不是全局对象的别名，
        // 因此同一批度量必须再挂一份，否则 `window.innerWidth` 读不到。
        if let Some(window) = context
            .global_object()
            .get(js_string!("window"), context)
            .ok()
            .and_then(|value| value.as_object())
        {
            let _ = window.define_property_or_throw(JsString::from(name), descriptor(), context);
        }
    }
    let _ = context.register_global_property(
        js_string!("devicePixelRatio"),
        JsValue::from(1),
        Attribute::READONLY,
    );
    if let Some(window) = context
        .global_object()
        .get(js_string!("window"), context)
        .ok()
        .and_then(|value| value.as_object())
    {
        let _ = window.set(
            js_string!("devicePixelRatio"),
            JsValue::from(1),
            false,
            context,
        );
    }
}

pub(crate) fn install_chrome(context: &mut Context, profile: &Profile) {
    if !profile.chrome_object {
        return;
    }
    // 嵌套对象都先在独立语句里建好：`ObjectInitializer` 独占 `&mut Context`。
    let platform_os = ObjectInitializer::new(context)
        .property(js_string!("WIN"), js_string!("win"), Attribute::all())
        .build();
    let platform_arch = ObjectInitializer::new(context)
        .property(js_string!("X86_64"), js_string!("x86-64"), Attribute::all())
        .build();
    let runtime = ObjectInitializer::new(context)
        .function(native(noop), js_string!("connect"), 0)
        .function(native(noop), js_string!("sendMessage"), 1)
        .property(js_string!("PlatformOs"), platform_os, Attribute::all())
        .property(js_string!("PlatformArch"), platform_arch, Attribute::all())
        .build();
    let install_state = ObjectInitializer::new(context)
        .property(
            js_string!("DISABLED"),
            js_string!("disabled"),
            Attribute::all(),
        )
        .property(
            js_string!("INSTALLED"),
            js_string!("installed"),
            Attribute::all(),
        )
        .property(
            js_string!("NOT_INSTALLED"),
            js_string!("not_installed"),
            Attribute::all(),
        )
        .build();
    let running_state = ObjectInitializer::new(context)
        .property(
            js_string!("CANNOT_RUN"),
            js_string!("cannot_run"),
            Attribute::all(),
        )
        .property(
            js_string!("READY_TO_RUN"),
            js_string!("ready_to_run"),
            Attribute::all(),
        )
        .property(
            js_string!("RUNNING"),
            js_string!("running"),
            Attribute::all(),
        )
        .build();
    let app = ObjectInitializer::new(context)
        .property(
            js_string!("isInstalled"),
            JsValue::from(false),
            Attribute::all(),
        )
        .function(native(noop), js_string!("getDetails"), 0)
        .function(native(noop), js_string!("getIsInstalled"), 0)
        .property(js_string!("InstallState"), install_state, Attribute::all())
        .property(js_string!("RunningState"), running_state, Attribute::all())
        .build();
    let chrome = ObjectInitializer::new(context)
        .property(js_string!("app"), app, Attribute::all())
        .property(js_string!("runtime"), runtime, Attribute::all())
        .function(native(noop), js_string!("csi"), 0)
        .function(native(noop), js_string!("loadTimes"), 0)
        .build();
    let _ =
        context.register_global_property(js_string!("chrome"), chrome.clone(), Attribute::all());
    // `window` 是独立对象，探测器走 `window.chrome` 时也要能读到。
    if let Some(window) = context
        .global_object()
        .get(js_string!("window"), context)
        .ok()
        .and_then(|value| value.as_object())
    {
        let _ = window.set(js_string!("chrome"), chrome, false, context);
    }
}

/// 监听器表按事件名存在实例自身上，前缀避免与真实属性撞车。
pub(crate) fn listener_key(type_name: &str) -> String {
    format!("__ornListener:{type_name}")
}

/// `permissions.query` 必须返回 Promise：探测器会 `await` 它。
///
/// 取值原则是**与其它 API 的存在性自洽**：报 `granted` 的权限，对应的 API 必须真的存在；
/// 不存在的 API 一律 `denied`。真实浏览器里 `geolocation` 报 granted 却读不到
/// `navigator.geolocation` 是不可能出现的组合，一句交叉校验就能识破。
pub(crate) fn perm_query(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .and_then(|value| value.as_object())
        .and_then(|descriptor| descriptor.get(js_string!("name"), context).ok())
        .and_then(|value| value.to_string(context).ok())
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_default();
    // 规范里 `name` 必须是已知的 PermissionName，否则 Promise 被 reject 成 TypeError。
    let Some(state) = permission_state(&name) else {
        let message = JsString::from(format!("'{name}' 不是有效的 PermissionName"));
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(message)),
            context,
        )?
        .into());
    };
    let result = ObjectInitializer::new(context)
        .property(js_string!("state"), js_string!(state), Attribute::all())
        .property(js_string!("name"), JsString::from(name), Attribute::all())
        .property(js_string!("onchange"), JsValue::null(), Attribute::all())
        .build();
    Ok(JsPromise::resolve(result, context)?.into())
}

/// 已知权限名 → 状态。`None` 表示这个名字不在规范里。
///
/// 状态与 API 存在性一一对应：沙箱里没有实现的能力（地理定位、媒体设备、通知）
/// 一律报 `denied` 或 `default`，避免出现「已授权但 API 不存在」的矛盾。
fn permission_state(name: &str) -> Option<&'static str> {
    match name {
        // 沙箱里确实存在 `navigator.permissions` 与 Clipboard 读写接口，
        // 但没有真实的系统授权流程：按「用户未拒绝」的默认态给 `prompt`。
        "clipboard-read" | "clipboard-write" => Some("prompt"),
        // 摄像头/麦克风：`navigator.mediaDevices` 未实现，报 granted 会自相矛盾。
        "camera" | "microphone" => Some("denied"),
        // 地理定位：`navigator.geolocation` 未实现。
        "geolocation" => Some("denied"),
        // 通知：接口存在但未申请，真实浏览器初始值就是 default。
        "notifications" => Some("default"),
        // 持久化存储：`navigator.storage` 未实现真实配额，按未授权处理。
        "persistent-storage" => Some("denied"),
        "midi" | "midi-sysex" => Some("denied"),
        "background-sync" => Some("denied"),
        "push" => Some("denied"),
        "screen-wake-lock" => Some("denied"),
        "window-management" => Some("denied"),
        "local-fonts" => Some("denied"),
        "idle-detection" => Some("denied"),
        "storage-access" => Some("denied"),
        "top-level-storage-access" => Some("denied"),
        _ => None,
    }
}

