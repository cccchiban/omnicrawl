
$ErrorActionPreference = "Stop"
# Windows PowerShell 的非交互 stdout 默认可能使用本地代码页；Python Host 固定按 UTF-8
# 读取，因此在脚本内明确统一输出编码，避免中文控件名被错误解码。
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8NoBom
$OutputEncoding = $utf8NoBom

function Get-ControlTypeMap {
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
    }
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
            left = [int][Math]::Round($rect.X)
            top = [int][Math]::Round($rect.Y)
            width = [int][Math]::Round($rect.Width)
            height = [int][Math]::Round($rect.Height)
        }
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
    $root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr][Int64]$request.window_handle)
    if ($null -eq $root) {
        throw "无法从 window_handle 获取 UI Automation 根元素。"
    }

    $condition = New-SearchCondition $request
    $matches = $root.FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        $condition
    )
    $matchCount = [int]$matches.Count

    if ([string]$request.action -eq "list") {
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
    else {
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
        }
    }

    [Console]::Out.Write((([PSCustomObject]@{ ok = $true; result = $result }) | ConvertTo-Json -Compress -Depth 8))
}
catch {
    [Console]::Out.Write((([PSCustomObject]@{ ok = $false; error = $_.Exception.Message }) | ConvertTo-Json -Compress -Depth 4))
    exit 1
}
