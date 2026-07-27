local state = {
    snapshot = nil,
    polling = false,
    lastGeneration = -1,
    lastError = nil,
    icons = {},
}

local function config(key, fallback)
    local value = noctalia.getConfig(key)
    if value == nil then
        return fallback
    end
    return value
end

local function idString(value)
    return string.format("%.0f", value or 0)
end

local function shellQuote(value)
    local text = tostring(value or "")
    return "'" .. string.gsub(text, "'", "'\"'\"'") .. "'"
end

local function backend()
    return tostring(config("backend_command", "niri-xmb"))
end

local function targetOutput()
    local configured = tostring(config("target_output", ""))
    if configured ~= "" then
        return configured
    end
    return barWidget.outputName() or noctalia.focusedOutputName() or ""
end

local function findOutput(name)
    if state.snapshot == nil or state.snapshot.outputs == nil then
        return nil
    end
    for _, output in ipairs(state.snapshot.outputs) do
        if output.name == name then
            return output
        end
    end
    return nil
end

local function runAction(arguments, callback)
    local command = backend() .. " " .. arguments
    if callback == nil then
        noctalia.runAsync(command)
    else
        noctalia.runAsync(command, callback)
    end
end

local function focusWindow(windowId)
    runAction("focus-window --window-id " .. idString(windowId))
end

local function focusWorkspace(workspaceId)
    runAction("focus-workspace --workspace-id " .. idString(workspaceId))
end

local function iconPath(appId, size)
    local key = tostring(appId or "") .. ":" .. tostring(size)
    local cached = state.icons[key]
    if cached ~= nil then
        return cached ~= false and cached or nil
    end
    local resolved = noctalia.appIconPath(tostring(appId or ""), size)
    state.icons[key] = resolved or false
    return resolved
end

local function parseTarget(value)
    local workspaceId, targetWindowId, fraction =
        string.match(tostring(value or ""), "^([^|]+)|([^|]+)|([^|]+)$")
    return tonumber(workspaceId), tonumber(targetWindowId), tonumber(fraction)
end

local function onDrop(payload, value, sceneX, sceneY)
    local sourceWindowId = tonumber(payload)
    local workspaceId, targetWindowId, fraction = parseTarget(value)
    if sourceWindowId == nil or workspaceId == nil or fraction == nil then
        noctalia.log("niri-xmb: rejected malformed drop payload")
        return
    end

    local arguments = "drop --source-window-id " .. idString(sourceWindowId)
        .. " --target-workspace-id " .. idString(workspaceId)
        .. " --fraction " .. tostring(fraction)
    if targetWindowId ~= nil and targetWindowId > 0 then
        arguments = arguments .. " --target-window-id " .. idString(targetWindowId)
    end

    if config("preserve_cursor_after_drag", true) then
        local host = findOutput(barWidget.outputName() or "")
        local localX = tonumber(sceneX)
        local localY = tonumber(sceneY)
        if host ~= nil and localX ~= nil and localY ~= nil then
            arguments = arguments
                .. " --cursor-x " .. tostring((host.x or 0) + localX)
                .. " --cursor-y " .. tostring((host.y or 0) + localY)
        end
    end

    runAction(arguments, function(_)
        state.lastGeneration = -1
    end)
end

local function marker(workspaceId, targetWindowId, fraction, key, height, slop)
    return ui.dropZone({
        key = key,
        width = 1,
        height = height,
        accepts = { "niri-window" },
        value = idString(workspaceId) .. "|" .. idString(targetWindowId) .. "|" .. tostring(fraction),
        onDrop = onDrop,
        hitSlop = slop,
        expandOnDrag = false,
        fill = "#00000000",
    })
end

local function windowVisual(window, iconSize)
    local path = iconPath(window.app_id, iconSize)
    local children = {}
    if path ~= nil then
        table.insert(children, ui.image({
            key = "image",
            path = path,
            width = iconSize,
            height = iconSize,
            fit = "contain",
            radius = 4,
        }))
    else
        table.insert(children, ui.glyph({
            key = "fallback",
            name = "apps",
            size = iconSize,
            color = window.urgent and "error" or "on_surface",
        }))
    end

    return children
end

local function windowItem(window, iconSize, allowDrag)
    local windowId = window.id
    local props = {
        key = "window-" .. idString(windowId),
        width = iconSize + 6,
        height = iconSize + 6,
        align = "center",
        justify = "center",
        padding = 3,
        radius = 7,
        fill = window.focused and "primary/0.20" or "#00000000",
        border = window.urgent and "error/0.9" or (window.focused and "primary/0.55" or "outline/0.16"),
        borderWidth = 1,
        opacity = window.floating and 0.78 or 1.0,
        onClick = function()
            focusWindow(windowId)
        end,
    }

    local children = windowVisual(window, iconSize)
    if allowDrag then
        props.dragType = "niri-window"
        props.payload = idString(windowId)
        props.tooltip = tostring(window.title or window.app_id or "")
        props.previewAncestor = 0
        props.liftFromLayout = false
        return ui.dragSource(props, children)
    end
    return ui.row(props, children)
end

local function workspaceNode(workspace, iconSize, allowDrag, allowJoin, showIndicator, inactiveOpacity)
    local windows = workspace.windows or {}
    local itemHeight = iconSize + 6
    local slop = itemHeight * 0.48
    local leftFraction = allowJoin and 0.5 or 0.25
    local rightFraction = allowJoin and 0.5 or 0.75
    local rowChildren = {}

    for index, window in ipairs(windows) do
        if allowDrag then
            table.insert(rowChildren, marker(
                workspace.id,
                window.id,
                leftFraction,
                "before-" .. idString(window.id),
                itemHeight,
                slop
            ))
        end
        table.insert(rowChildren, windowItem(window, iconSize, allowDrag))
        if allowDrag and index == #windows then
            table.insert(rowChildren, marker(
                workspace.id,
                window.id,
                rightFraction,
                "after-" .. idString(window.id),
                itemHeight,
                slop
            ))
        end
    end

    if #windows == 0 then
        table.insert(rowChildren, ui.box({
            key = "empty-" .. idString(workspace.id),
            width = 10,
            height = 10,
            radius = 5,
            fill = workspace.active and "primary/0.85" or "on_surface_variant/0.35",
            onClick = function()
                focusWorkspace(workspace.id)
            end,
        }))
    end

    local groupChildren = {
        ui.row({
            key = "windows",
            align = "center",
            gap = 1,
        }, rowChildren),
    }
    if showIndicator then
        table.insert(groupChildren, ui.box({
            key = "indicator",
            height = 2,
            radius = 1,
            fill = workspace.active and "primary" or "#00000000",
        }))
    end

    local props = {
        key = "workspace-" .. idString(workspace.id),
        direction = "column",
        align = "stretch",
        gap = 1,
        paddingH = 3,
        paddingV = 2,
        radius = 9,
        fill = workspace.active and "primary/0.10" or "surface_variant/0.20",
        border = workspace.urgent and "error/0.9" or (workspace.active and "primary/0.42" or "outline/0.18"),
        borderWidth = 1,
        opacity = workspace.active and 1.0 or inactiveOpacity,
    }

    if allowDrag then
        props.accepts = { "niri-window" }
        props.value = idString(workspace.id) .. "|0|0.5"
        props.onDrop = onDrop
        props.hitSlop = 0
        props.expandOnDrag = false
        return ui.dropZone(props, groupChildren)
    end
    return ui.column(props, groupChildren)
end

local function render()
    local output = findOutput(targetOutput())
    if output == nil then
        barWidget.render(ui.row({
            key = "niri-xmb-missing",
            align = "center",
            gap = 4,
            paddingH = 4,
        }, {
            ui.glyph({
                key = "warning",
                name = "alert-circle",
                size = 18,
                color = "error",
            }),
        }))
        return
    end

    local iconSize = math.floor(config("icon_size", 22))
    local allowDrag = config("allow_drag", true)
    local allowJoin = config("allow_column_join", false)
    local showIndicator = config("show_active_indicator", true)
    local hideEmpty = config("hide_empty_workspaces", false)
    local inactiveOpacity = config("group_opacity", 0.72)
    local children = {}

    for _, workspace in ipairs(output.workspaces or {}) do
        local windows = workspace.windows or {}
        if not hideEmpty or workspace.active or #windows > 0 then
            table.insert(children, workspaceNode(
                workspace,
                iconSize,
                allowDrag,
                allowJoin,
                showIndicator,
                inactiveOpacity
            ))
        end
    end

    barWidget.render(ui.row({
        key = "niri-xmb-root",
        align = "center",
        gap = 3,
    }, children))
end

local function poll(force)
    if state.polling then
        return
    end
    state.polling = true
    local started = noctalia.runAsync(backend() .. " snapshot", function(result)
        state.polling = false
        if result.exitCode ~= 0 then
            local message = tostring(result.stderr or result.stdout or "snapshot failed")
            if state.lastError ~= message then
                state.lastError = message
                noctalia.log("niri-xmb snapshot failed: " .. message)
            end
            render()
            return
        end

        local decoded, errorMessage = noctalia.json.decode(result.stdout or "")
        if decoded == nil then
            state.lastError = tostring(errorMessage or "invalid JSON")
            noctalia.log("niri-xmb snapshot JSON failed: " .. state.lastError)
            render()
            return
        end

        state.lastError = nil
        state.snapshot = decoded
        if force or decoded.generation ~= state.lastGeneration then
            state.lastGeneration = decoded.generation
            render()
        end
    end)
    if not started then
        -- The first top-level poll can run before Noctalia has attached the
        -- async result handler to a newly-created plugin host. Do not leave the
        -- poll latch stuck: update() will retry after the host is ready.
        state.polling = false
    end
end

function update()
    poll(false)
end

function onScroll(axis, steps, startsGesture)
    if axis ~= "vertical" or steps == 0 then
        return
    end
    local delta = steps > 0 and 1 or -1
    runAction("cycle-workspace --output " .. shellQuote(targetOutput()) .. " --delta " .. tostring(delta))
end

function onConfigChanged()
    state.icons = {}
    state.lastGeneration = -1
    noctalia.setUpdateInterval(config("poll_interval_ms", 250))
    poll(true)
end

noctalia.setUpdateInterval(config("poll_interval_ms", 250))
render()
poll(true)
