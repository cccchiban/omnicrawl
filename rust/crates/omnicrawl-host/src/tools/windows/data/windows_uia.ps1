
$ErrorActionPreference = "Stop"
# Windows PowerShell 的非交互 stdout 默认可能使用本地代码页；Python Host 固定按 UTF-8
# 读取，因此在脚本内明确统一输出编码，避免中文控件名被错误解码。
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8NoBom
$OutputEncoding = $utf8NoBom

function Get-ControlTypeMap {
    # 与 `control.rs` 的 CONTROL_TYPES 一一对应：tree 会上报这些类型，list 必须能按它们过滤。
    return @{
        "button" = [System.Windows.Automation.ControlType]::Button
        "checkbox" = [System.Windows.Automation.ControlType]::CheckBox
        "combobox" = [System.Windows.Automation.ControlType]::ComboBox
        "edit" = [System.Windows.Automation.ControlType]::Edit
        "hyperlink" = [System.Windows.Automation.ControlType]::Hyperlink
        "list" = [System.Windows.Automation.ControlType]::List
        "listitem" = [System.Windows.Automation.ControlType]::ListItem
        "menu" = [System.Windows.Automation.ControlType]::Menu
        "menuitem" = [System.Windows.Automation.ControlType]::MenuItem
        "radiobutton" = [System.Windows.Automation.ControlType]::RadioButton
        "tab" = [System.Windows.Automation.ControlType]::Tab
        "tabitem" = [System.Windows.Automation.ControlType]::TabItem
        "text" = [System.Windows.Automation.ControlType]::Text
        "tree" = [System.Windows.Automation.ControlType]::Tree
        "treeitem" = [System.Windows.Automation.ControlType]::TreeItem
        "window" = [System.Windows.Automation.ControlType]::Window
        "pane" = [System.Windows.Automation.ControlType]::Pane
        "document" = [System.Windows.Automation.ControlType]::Document
        "custom" = [System.Windows.Automation.ControlType]::Custom
        "group" = [System.Windows.Automation.ControlType]::Group
        "calendar" = [System.Windows.Automation.ControlType]::Calendar
        "image" = [System.Windows.Automation.ControlType]::Image
        "slider" = [System.Windows.Automation.ControlType]::Slider
        "spinner" = [System.Windows.Automation.ControlType]::Spinner
        "progressbar" = [System.Windows.Automation.ControlType]::ProgressBar
        "splitbutton" = [System.Windows.Automation.ControlType]::SplitButton
        "scrollbar" = [System.Windows.Automation.ControlType]::ScrollBar
        "table" = [System.Windows.Automation.ControlType]::Table
        "datagrid" = [System.Windows.Automation.ControlType]::DataGrid
        "dataitem" = [System.Windows.Automation.ControlType]::DataItem
        "header" = [System.Windows.Automation.ControlType]::Header
        "headeritem" = [System.Windows.Automation.ControlType]::HeaderItem
        "menubar" = [System.Windows.Automation.ControlType]::MenuBar
        "toolbar" = [System.Windows.Automation.ControlType]::ToolBar
        "statusbar" = [System.Windows.Automation.ControlType]::StatusBar
        "tooltip" = [System.Windows.Automation.ControlType]::ToolTip
        "thumb" = [System.Windows.Automation.ControlType]::Thumb
        "separator" = [System.Windows.Automation.ControlType]::Separator
        "titlebar" = [System.Windows.Automation.ControlType]::TitleBar
    }
}

function ConvertTo-Pixel([double]$Value) {
    # 无布局元素（尚未完成布局的 View、离屏 RepeatButton 等）的 BoundingRectangle 是
    # Rect.Empty：X/Y 为 ±∞、Width/Height 为 0。直接 [int][Math]::Round 会抛
    # “Cannot convert value "∞" to type System.Int32”，把整次控件查询带崩。
    # 这里统一收敛成 0：坐标不可知就用 0 表示，比让整批结果失败有用。
    if ([double]::IsNaN($Value) -or [double]::IsInfinity($Value)) {
        return 0
    }
    $rounded = [Math]::Round($Value)
    if ($rounded -gt [int]::MaxValue) { return [int]::MaxValue }
    if ($rounded -lt [int]::MinValue) { return [int]::MinValue }
    return [int]$rounded
}

function Get-ElementSummary([System.Windows.Automation.AutomationElement]$Element) {
    $rect = $Element.Current.BoundingRectangle
    $controlTypeName = [string]$Element.Current.ControlType.ProgrammaticName
    if ($controlTypeName.StartsWith("ControlType.")) {
        $controlTypeName = $controlTypeName.Substring("ControlType.".Length)
    }

    return [PSCustomObject]@{
        name = [string]$Element.Current.Name
        automation_id = [string]$Element.Current.AutomationId
        class_name = [string]$Element.Current.ClassName
        control_type = $controlTypeName.ToLowerInvariant()
        process_id = [int]$Element.Current.ProcessId
        is_enabled = [bool]$Element.Current.IsEnabled
        is_offscreen = [bool]$Element.Current.IsOffscreen
        bounds = [PSCustomObject]@{
            left = ConvertTo-Pixel $rect.X
            top = ConvertTo-Pixel $rect.Y
            width = ConvertTo-Pixel $rect.Width
            height = ConvertTo-Pixel $rect.Height
        }
    }
}

function Get-ElementPatternNames([System.Windows.Automation.AutomationElement]$Element) {
    # 只报本工具支持的四种操作对应的 pattern。模型据此判断哪个 action 会成功，
    # 不必逐个试探（每次试探都是一次失败的工具调用）。
    $named = @{
        "InvokePatternIdentifiers.Pattern" = "invoke"
        "ValuePatternIdentifiers.Pattern" = "value"
        "SelectionItemPatternIdentifiers.Pattern" = "select"
        "TogglePatternIdentifiers.Pattern" = "toggle"
    }
    $present = @{}
    # 单个元素的 pattern 查询会因跨进程提供程序失效而抛异常：放过它，别带崩整棵树。
    try {
        foreach ($pattern in $Element.GetSupportedPatterns()) {
            $name = $named[[string]$pattern.ProgrammaticName]
            if ($null -ne $name) {
                $present[$name] = $true
            }
        }
    }
    catch {
    }
    $names = New-Object "System.Collections.Generic.List[string]"
    foreach ($name in @("invoke", "value", "select", "toggle")) {
        if ($present.ContainsKey($name)) {
            $names.Add($name)
        }
    }
    return @($names.ToArray())
}

function Get-ElementValueText([System.Windows.Automation.AutomationElement]$Element) {
    # 值模式让模型「读回输入框内容」而不必截图；密码框一律不读。
    # 值可能是整篇文档（记事本的 document 元素就是全文），必须在这里先收口，
    # 否则一条树行会把整份文件塞进工具结果。
    $limit = 160
    try {
        if ([bool]$Element.Current.IsPassword) {
            return $null
        }
        $pattern = $null
        if (-not $Element.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$pattern)) {
            return $null
        }
        $text = [string]$pattern.Current.Value
        $text = $text.Trim()
        if ($text.Length -gt $limit) {
            return $text.Substring(0, $limit) + "…"
        }
        return $text
    }
    catch {
        return $null
    }
}

function Get-ElementToggleState([System.Windows.Automation.AutomationElement]$Element) {
    try {
        $pattern = $null
        if (-not $Element.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$pattern)) {
            return $null
        }
        return ([string]$pattern.Current.ToggleState).ToLowerInvariant()
    }
    catch {
        return $null
    }
}

function Get-ElementLiveState([System.Windows.Automation.AutomationElement]$Element) {
    # 动作后立刻回读的可核对状态：模型用它确认效果，就不要截图了。
    return [PSCustomObject]@{
        patterns = @(Get-ElementPatternNames $Element)
        value = Get-ElementValueText $Element
        checked = Get-ElementToggleState $Element
    }
}

function Test-NoiseNode($Summary, $Patterns) {
    # 无名称、无 AutomationId、无可用 pattern 的容器节点只是分层外壳，
    # 留着会让树里一半行都是 `pane`。跳过这一层，子树提到上一层。
    if ([string]$Summary.name -ne "" -or [string]$Summary.automation_id -ne "") { return $false }
    if (@($Patterns).Count -gt 0) { return $false }
    return @("pane", "custom", "group", "text") -contains [string]$Summary.control_type
}

function Add-TreeNodes {
    param(
        [System.Windows.Automation.AutomationElement]$Element,
        [int]$Depth,
        [int]$MaxDepth,
        $Nodes,
        [int]$NodeLimit
    )
    if ($Nodes.Count -ge $NodeLimit) { return }

    $summary = $null
    try {
        $summary = Get-ElementSummary $Element
    }
    catch {
        # 元素在遍历途中消失（页面重绘、菜单收起）：跳过这一支，别中断整棵树。
        return
    }
    $patterns = @(Get-ElementPatternNames $Element)

    if (Test-NoiseNode $summary $patterns) {
        # 外壳节点不占位，但它的子节点仍按当前深度展开。
        $childDepth = $Depth
    }
    else {
        $Nodes.Add([PSCustomObject]@{
            depth = $Depth
            control_type = $summary.control_type
            name = $summary.name
            automation_id = $summary.automation_id
            class_name = $summary.class_name
            is_enabled = $summary.is_enabled
            is_offscreen = $summary.is_offscreen
            bounds = $summary.bounds
            patterns = $patterns
            value = Get-ElementValueText $Element
            checked = Get-ElementToggleState $Element
        })
        $childDepth = $Depth + 1
    }

    if ($childDepth -gt $MaxDepth) { return }
    # 离屏分支整棵不展开：看不见也点不到，展开只会白花 token。
    if ($summary.is_offscreen) { return }
    if ($Nodes.Count -ge $NodeLimit) { return }

    $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
    $child = $walker.GetFirstChild($Element)
    while ($null -ne $child) {
        Add-TreeNodes -Element $child -Depth $childDepth -MaxDepth $MaxDepth -Nodes $Nodes -NodeLimit $NodeLimit
        if ($Nodes.Count -ge $NodeLimit) { return }
        $child = $walker.GetNextSibling($child)
    }
}

function New-SearchCondition($Request) {
    $conditions = New-Object "System.Collections.Generic.List[System.Windows.Automation.Condition]"
    if ($null -ne $Request.name -and [string]$Request.name -ne "") {
        $conditions.Add([System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::NameProperty,
            [string]$Request.name
        ))
    }
    if ($null -ne $Request.automation_id -and [string]$Request.automation_id -ne "") {
        $conditions.Add([System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::AutomationIdProperty,
            [string]$Request.automation_id
        ))
    }
    if ($null -ne $Request.class_name -and [string]$Request.class_name -ne "") {
        $conditions.Add([System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ClassNameProperty,
            [string]$Request.class_name
        ))
    }
    if ($null -ne $Request.control_type -and [string]$Request.control_type -ne "") {
        $controlTypeMap = Get-ControlTypeMap
        $controlType = $controlTypeMap[[string]$Request.control_type]
        if ($null -eq $controlType) {
            throw "不支持的 control_type：$($Request.control_type)"
        }
        $conditions.Add([System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            $controlType
        ))
    }

    $condition = [System.Windows.Automation.Condition]::TrueCondition
    foreach ($item in $conditions) {
        $condition = [System.Windows.Automation.AndCondition]::new($condition, $item)
    }
    return $condition
}

try {
    Add-Type -AssemblyName UIAutomationClient
    $encoded = [string]$env:OMNICRAWL_UIA_REQUEST_B64
    if ([string]::IsNullOrWhiteSpace($encoded)) {
        throw "未收到 UI Automation 请求。"
    }
    $raw = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($encoded))
    $request = $raw | ConvertFrom-Json
    $handle = [Int64]$request.window_handle
    if ($handle -le 0) {
        throw "window_handle 无效：$($request.window_handle)。"
    }
    try {
        $root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$handle)
    }
    catch {
        # FromHandle 对失效句柄只抛 "Unrecognized error."，补上可操作的说明。
        throw "窗口句柄 $handle 已失效：目标窗口可能已关闭。先用 windows_window 的 list 重新取句柄。"
    }
    if ($null -eq $root) {
        throw "窗口句柄 $handle 已失效：目标窗口可能已关闭。先用 windows_window 的 list 重新取句柄。"
    }

    $condition = New-SearchCondition $request
    $matchCount = 0

    if ([string]$request.action -eq "list") {
        $matches = $root.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            $condition
        )
        $matchCount = [int]$matches.Count
        $limit = [Math]::Min($matchCount, [int]$request.max_results)
        $items = New-Object "System.Collections.Generic.List[object]"
        for ($index = 0; $index -lt $limit; $index++) {
            $items.Add((Get-ElementSummary $matches.Item($index)))
        }
        $result = [PSCustomObject]@{
            action = "list"
            matched_count = $matchCount
            truncated = [bool]($matchCount -gt $limit)
            controls = @($items.ToArray())
        }
    }
    elseif ([string]$request.action -eq "tree") {
        # 控件树：一次取回整窗语义结构，模型据此直接定位，不必截图猜坐标。
        # 多取一个节点用来判定截断，再裁回上限。
        $limit = [int]$request.max_results
        $nodes = New-Object "System.Collections.Generic.List[object]"
        Add-TreeNodes -Element $root -Depth 0 -MaxDepth ([int]$request.max_depth) -Nodes $nodes -NodeLimit ($limit + 1)
        $truncated = [bool]($nodes.Count -gt $limit)
        if ($truncated) {
            $nodes.RemoveRange($limit, $nodes.Count - $limit)
        }
        $result = [PSCustomObject]@{
            action = "tree"
            node_count = [int]$nodes.Count
            truncated = $truncated
            max_depth = [int]$request.max_depth
            nodes = @($nodes.ToArray())
        }
    }
    else {
        $matches = $root.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            $condition
        )
        $matchCount = [int]$matches.Count
        if ($matchCount -eq 0) {
            throw "未找到匹配的 UI 控件。"
        }
        if ($null -eq $request.index) {
            if ($matchCount -ne 1) {
                throw "定位条件匹配到 $matchCount 个控件；请先 list，或提供 index。"
            }
            $selectedIndex = 0
        }
        else {
            $selectedIndex = [int]$request.index
            if ($selectedIndex -lt 0 -or $selectedIndex -ge $matchCount) {
                throw "index 超出匹配范围：0 到 $($matchCount - 1)。"
            }
        }

        $element = $matches.Item($selectedIndex)
        $availablePatterns = @(Get-ElementPatternNames $element)
        $requiredPattern = @{
            "invoke" = "invoke"
            "set_value" = "value"
            "select" = "select"
            "toggle" = "toggle"
        }[[string]$request.action]
        if ($null -ne $requiredPattern -and -not ($availablePatterns -contains $requiredPattern)) {
            # 提前给出可用操作，比让 GetCurrentPattern 抛 "Unsupported Pattern." 有用。
            $listed = if ($availablePatterns.Count -eq 0) { "无" } else { $availablePatterns -join "、" }
            throw "控件不支持 $($request.action) 操作（可用：$listed）。控件：$($element.Current.ControlType.ProgrammaticName) `"$($element.Current.Name)`"。"
        }
        switch ([string]$request.action) {
            "invoke" {
                $pattern = [System.Windows.Automation.InvokePattern]$element.GetCurrentPattern(
                    [System.Windows.Automation.InvokePattern]::Pattern
                )
                $pattern.Invoke()
            }
            "set_value" {
                if ($null -eq $request.value) {
                    throw "set_value 必须提供 value。"
                }
                $pattern = [System.Windows.Automation.ValuePattern]$element.GetCurrentPattern(
                    [System.Windows.Automation.ValuePattern]::Pattern
                )
                $pattern.SetValue([string]$request.value)
            }
            "select" {
                $pattern = [System.Windows.Automation.SelectionItemPattern]$element.GetCurrentPattern(
                    [System.Windows.Automation.SelectionItemPattern]::Pattern
                )
                $pattern.Select()
            }
            "toggle" {
                $pattern = [System.Windows.Automation.TogglePattern]$element.GetCurrentPattern(
                    [System.Windows.Automation.TogglePattern]::Pattern
                )
                $pattern.Toggle()
            }
            "focus" {
                $element.SetFocus()
            }
            default {
                throw "不支持的 UI 控件操作：$($request.action)"
            }
        }
        $result = [PSCustomObject]@{
            action = [string]$request.action
            index = $selectedIndex
            target = Get-ElementSummary $element
            state = Get-ElementLiveState $element
        }
    }

    [Console]::Out.Write((([PSCustomObject]@{ ok = $true; result = $result }) | ConvertTo-Json -Compress -Depth 8))
}
catch {
    [Console]::Out.Write((([PSCustomObject]@{ ok = $false; error = $_.Exception.Message }) | ConvertTo-Json -Compress -Depth 4))
    exit 1
}
