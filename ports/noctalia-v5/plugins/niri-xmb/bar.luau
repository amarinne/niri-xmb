local state = {
    snapshot = nil,
    polling = false,
    lastGeneration = -1,
    lastError = nil,
    icons = {},
    iconRetryAt = {},
    hoveredWindowId = nil,
    pendingDrop = nil,
    dropErrorUntil = nil,
}

local render
local poll

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

local function command(arguments)
    local parts = { shellQuote(backend()) }
    for _, argument in ipairs(arguments or {}) do
        table.insert(parts, shellQuote(argument))
    end
    return table.concat(parts, " ")
end

local function finiteNumber(value)
    local number = tonumber(value)
    if number == nil or number ~= number or number == math.huge or number == -math.huge then
        return nil
    end
    return number
end

local function positiveId(value)
    local number = finiteNumber(value)
    if number == nil or number <= 0 or number ~= math.floor(number) then
        return nil
    end
    return number
end

local function optionalId(value)
    local number = finiteNumber(value)
    if number == nil or number < 0 or number ~= math.floor(number) then
        return nil
    end
    return number
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

local function findWorkspaceIn(snapshot, workspaceId)
    if snapshot == nil or snapshot.outputs == nil then
        return nil, nil
    end
    for _, output in ipairs(snapshot.outputs) do
        for _, workspace in ipairs(output.workspaces or {}) do
            if workspace.id == workspaceId then
                return output, workspace
            end
        end
    end
    return nil, nil
end

local function findWorkspace(workspaceId)
    return findWorkspaceIn(state.snapshot, workspaceId)
end

local function findWindowIn(snapshot, windowId)
    if snapshot == nil or snapshot.outputs == nil then
        return nil, nil, nil, nil
    end
    for _, output in ipairs(snapshot.outputs) do
        for _, workspace in ipairs(output.workspaces or {}) do
            for index, window in ipairs(workspace.windows or {}) do
                if window.id == windowId then
                    return output, workspace, index, window
                end
            end
        end
    end
    return nil, nil, nil, nil
end

local function findWindow(windowId)
    return findWindowIn(state.snapshot, windowId)
end

local function runAction(arguments, callback)
    local invocation = command(arguments)
    if callback == nil then
        noctalia.runAsync(invocation)
    else
        noctalia.runAsync(invocation, callback)
    end
end

local function focusWindow(windowId)
    local validatedId = positiveId(windowId)
    if validatedId == nil then
        noctalia.log("niri-xmb: rejected invalid window ID")
        return
    end
    runAction({ "focus-window", "--window-id", idString(validatedId) })
end

local function focusWorkspace(workspaceId)
    local validatedId = positiveId(workspaceId)
    if validatedId == nil then
        noctalia.log("niri-xmb: rejected invalid workspace ID")
        return
    end
    runAction({ "focus-workspace", "--workspace-id", idString(validatedId) })
end

local function trim(value)
    return string.match(tostring(value or ""), "^%s*(.-)%s*$") or ""
end

local function addIconCandidate(candidates, seen, value)
    local candidate = trim(value)
    if candidate == "" then
        return
    end

    if not seen[candidate] then
        seen[candidate] = true
        table.insert(candidates, candidate)
    end

    local desktopStem = string.match(candidate, "([^/]+)%.desktop$")
    if desktopStem ~= nil and not seen[desktopStem] then
        seen[desktopStem] = true
        table.insert(candidates, desktopStem)
    end
end

local function iconCandidates(window)
    local candidates = {}
    local seen = {}

    -- Accept common field names so richer snapshots can provide desktop
    -- metadata without another plugin rewrite.
    addIconCandidate(candidates, seen, window.icon_name)
    addIconCandidate(candidates, seen, window.iconName)
    addIconCandidate(candidates, seen, window.desktop_entry)
    addIconCandidate(candidates, seen, window.desktopEntry)
    addIconCandidate(candidates, seen, window.desktop_id)
    addIconCandidate(candidates, seen, window.desktopId)
    addIconCandidate(candidates, seen, window.desktop_file)
    addIconCandidate(candidates, seen, window.desktopFile)

    local appId = trim(window.app_id or window.appId)
    addIconCandidate(candidates, seen, appId)

    local lowerAppId = string.lower(appId)
    for _, suffix in ipairs({ "-terminal", "_terminal", ".terminal", "-window", "_window" }) do
        if string.sub(lowerAppId, -#suffix) == suffix then
            addIconCandidate(candidates, seen, string.sub(appId, 1, #appId - #suffix))
        end
    end

    local tail = string.match(appId, "([^./]+)$")
    if tail ~= appId then
        addIconCandidate(candidates, seen, tail)
    end

    -- Use a title only when it is a stable icon-like token. Dynamic browser
    -- and terminal titles must not create an unbounded negative cache.
    local title = trim(window.title)
    if #title <= 64 and string.match(title, "^[%w._+%-]+$") ~= nil then
        addIconCandidate(candidates, seen, title)
    end
    return candidates
end

local function resolveIconCandidate(candidate, size)
    local key = candidate .. ":" .. tostring(size)
    local cached = state.icons[key]
    if cached ~= nil then
        return cached
    end

    local now = noctalia.nowMs()
    local retryAt = state.iconRetryAt[key]
    if retryAt ~= nil and now < retryAt then
        return nil
    end

    -- Resolve a larger source than the logical display size. Noctalia then
    -- downsamples the image instead of enlarging a low-resolution icon on a
    -- fractional-scale output.
    local decodeSize = math.max(64, size * 2)
    local resolved = noctalia.appIconPath(candidate, decodeSize)
    if resolved ~= nil then
        state.icons[key] = resolved
        state.iconRetryAt[key] = nil
    else
        -- Desktop entries can finish loading after the first plugin frame.
        -- Retry misses slowly instead of keeping a startup placeholder forever.
        state.iconRetryAt[key] = now + 5000
    end
    return resolved
end

local function iconPath(window, size)
    for _, candidate in ipairs(iconCandidates(window)) do
        local resolved = resolveIconCandidate(candidate, size)
        if resolved ~= nil then
            return resolved
        end
    end
    return nil
end

local function parseTarget(value)
    local workspaceId, targetWindowId, fraction =
        string.match(tostring(value or ""), "^([^|]+)|([^|]+)|([^|]+)$")
    return positiveId(workspaceId), optionalId(targetWindowId), finiteNumber(fraction)
end

local function applyOptimisticDrop(sourceWindowId, workspaceId, targetWindowId, fraction)
    local _, sourceWorkspace, sourceIndex, sourceWindow = findWindow(sourceWindowId)
    local _, targetWorkspace = findWorkspace(workspaceId)
    if sourceWorkspace == nil or sourceIndex == nil or sourceWindow == nil or targetWorkspace == nil then
        return false
    end
    if targetWindowId == sourceWindowId then
        return false
    end

    table.remove(sourceWorkspace.windows, sourceIndex)

    local insertIndex = #(targetWorkspace.windows or {}) + 1
    if targetWindowId ~= nil and targetWindowId > 0 then
        for index, window in ipairs(targetWorkspace.windows or {}) do
            if window.id == targetWindowId then
                insertIndex = index
                if fraction >= 0.5 then
                    insertIndex = insertIndex + 1
                end
                break
            end
        end
    end

    sourceWindow.workspace_id = workspaceId
    sourceWindow.floating = false
    table.insert(targetWorkspace.windows, insertIndex, sourceWindow)
    -- The optimistic order is a preview, not the truth. It holds only until
    -- the daemon answers for this drop or this deadline passes; whichever
    -- comes first restores canonical state.
    state.pendingDrop = {
        sourceWindowId = sourceWindowId,
        workspaceId = workspaceId,
        targetWindowId = targetWindowId,
        fraction = fraction,
        submittedAt = noctalia.nowMs(),
        deadline = noctalia.nowMs() + 3000,
    }
    state.lastGeneration = -1
    render()
    return true
end

local function pendingDropMatches(snapshot, pending)
    local _, workspace, sourceIndex = findWindowIn(snapshot, pending.sourceWindowId)
    if workspace == nil or sourceIndex == nil or workspace.id ~= pending.workspaceId then
        return false
    end
    if pending.targetWindowId == nil or pending.targetWindowId <= 0 then
        return true
    end
    local _, targetWorkspace, targetIndex = findWindowIn(snapshot, pending.targetWindowId)
    if targetWorkspace == nil or targetIndex == nil or targetWorkspace.id ~= pending.workspaceId then
        return false
    end
    if pending.fraction < 0.5 then
        return sourceIndex + 1 == targetIndex
    end
    return sourceIndex == targetIndex + 1
end

local function onDrop(payload, value, sceneX, sceneY)
    local sourceWindowId = positiveId(payload)
    local workspaceId, targetWindowId, fraction = parseTarget(value)
    if sourceWindowId == nil or workspaceId == nil or targetWindowId == nil
        or fraction == nil or fraction < 0 or fraction > 1 then
        noctalia.log("niri-xmb: rejected malformed drop payload")
        return
    end

    applyOptimisticDrop(sourceWindowId, workspaceId, targetWindowId, fraction)

    local arguments = {
        "drop",
        "--source-window-id", idString(sourceWindowId),
        "--target-workspace-id", idString(workspaceId),
        "--fraction", tostring(fraction),
    }
    if targetWindowId > 0 then
        table.insert(arguments, "--target-window-id")
        table.insert(arguments, idString(targetWindowId))
    end

    if config("preserve_cursor_after_drag", true) then
        local host = findOutput(barWidget.outputName() or "")
        local localX = finiteNumber(sceneX)
        local localY = finiteNumber(sceneY)
        if host ~= nil and localX ~= nil and localY ~= nil then
            local hostX = finiteNumber(host.x) or 0
            local hostY = finiteNumber(host.y) or 0
            local cursorX = finiteNumber(hostX + localX)
            local cursorY = finiteNumber(hostY + localY)
            if cursorX ~= nil and cursorY ~= nil then
                table.insert(arguments, "--cursor-x")
                table.insert(arguments, tostring(cursorX))
                table.insert(arguments, "--cursor-y")
                table.insert(arguments, tostring(cursorY))
            end
        end
    end

    local function commitDrop()
        runAction(arguments, function(result)
            local response = trim(tostring(result.stdout or result.stderr or ""))
            -- The daemon answers only after the move is confirmed, the action
            -- fails, or confirmation times out. Either way this reply ends the
            -- optimistic preview and pulls canonical state immediately.
            local pending = state.pendingDrop
            local pendingAge = nil
            if pending ~= nil and finiteNumber(pending.submittedAt) ~= nil then
                pendingAge = math.max(0, math.floor(noctalia.nowMs() - pending.submittedAt))
            end
            state.pendingDrop = nil
            state.lastGeneration = -1
            if result.exitCode ~= 0 or string.match(response, "^error:") ~= nil then
                -- One line carries every field needed to reproduce the
                -- rejection against daemon logs: ids and numbers only.
                noctalia.log(string.format(
                    "niri-xmb drop failed: source=%s workspace=%s target=%s fraction=%s generation=%s pendingAgeMs=%s response=%s",
                    idString(sourceWindowId),
                    idString(workspaceId),
                    idString(targetWindowId),
                    tostring(fraction),
                    idString(state.snapshot ~= nil and state.snapshot.generation or nil),
                    pendingAge ~= nil and tostring(pendingAge) or "unknown",
                    response ~= "" and response or "(empty)"
                ))
                state.dropErrorUntil = noctalia.nowMs() + 2500
                render()
            end
            poll(true)
        end)
    end

    commitDrop()
end

local function onSourceDrop(payload, value, _, _)
    local sourceWindowId = positiveId(payload)
    local workspaceId, targetWindowId, fraction = parseTarget(value)
    if sourceWindowId == nil or workspaceId == nil or targetWindowId == nil
        or fraction == nil or fraction < 0 or fraction > 1 then
        return
    end
    local target = findWorkspace(workspaceId)
    if target == nil or target.name == targetOutput() then
        return
    end
    applyOptimisticDrop(sourceWindowId, workspaceId, targetWindowId, fraction)
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

local function windowVisual(window, iconSize, indicatorMode)
    local path = iconPath(window, iconSize)
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

    if window.focused and indicatorMode ~= "none" then
        local indicator
        local indicatorSize
        if indicatorMode == "underline" then
            indicatorSize = 2
            indicator = ui.box({
                key = "active-underline",
                width = math.max(8, math.floor(iconSize * 0.62 + 0.5)),
                height = indicatorSize,
                radius = indicatorSize / 2,
                fill = "primary",
            })
        else
            indicatorSize = math.max(4, math.floor(iconSize * 0.32 + 0.5))
            indicator = ui.box({
                key = "active-dot",
                width = indicatorSize,
                height = indicatorSize,
                radius = indicatorSize / 2,
                fill = "primary",
            })
        end
        return {
            ui.column({
                key = "focused-content-" .. indicatorMode,
                width = iconSize,
                height = iconSize,
                align = "center",
                justify = "center",
                gap = -indicatorSize,
            }, {
                children[1],
                indicator,
            }),
        }
    end

    return children
end

local function windowItem(window, iconSize, allowDrag, indicatorMode)
    local windowId = window.id
    local hovered = state.hoveredWindowId == windowId
    local props = {
        key = "window-" .. idString(windowId),
        width = iconSize + 6,
        height = iconSize + 6,
        align = "center",
        justify = "center",
        padding = 3,
        radius = (iconSize + 6) / 2,
        fill = hovered and "on_surface/0.10" or "#00000000",
        border = "#00000000",
        borderWidth = 0,
        opacity = window.floating and 0.78 or 1.0,
        onClick = function()
            focusWindow(windowId)
        end,
        onHover = function(value)
            local nextId = tostring(value) == "true" and windowId or nil
            if state.hoveredWindowId ~= nextId then
                state.hoveredWindowId = nextId
                render()
            end
        end,
    }

    local children = windowVisual(window, iconSize, indicatorMode)
    if allowDrag then
        props.dragType = "niri-window"
        props.payload = idString(windowId)
        props.onDragEnd = onSourceDrop
        props.tooltip = tostring(window.title or window.app_id or "")
        props.previewAncestor = 0
        props.liftFromLayout = false
        return ui.dragSource(props, children)
    end
    return ui.row(props, children)
end

local function workspaceNode(workspace, iconSize, allowDrag, allowJoin, indicatorMode, inactiveOpacity)
    local windows = workspace.windows or {}
    local itemHeight = iconSize + 6
    -- Marker positions are one item plus one marker apart. Use a small overlap
    -- so every point on an icon resolves to its left or right insertion zone.
    local slop = (itemHeight + 1) * 0.5 + 0.5
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
        table.insert(rowChildren, windowItem(window, iconSize, allowDrag, indicatorMode))
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
        table.insert(rowChildren, ui.row({
            key = "empty-" .. idString(workspace.id),
            width = itemHeight,
            height = itemHeight,
            align = "center",
            justify = "center",
            radius = itemHeight / 2,
            fill = "#00000000",
            onClick = function()
                focusWorkspace(workspace.id)
            end,
        }, {
            ui.row({
                key = "empty-dot",
                width = 8,
                height = 8,
                radius = 4,
                fill = workspace.active and "primary" or "on_surface_variant/0.55",
            }),
        }))
    end

    local urgent = false
    for _, window in ipairs(windows) do
        if window.urgent then
            urgent = true
            break
        end
    end

    local groupFill = workspace.active and "surface_variant/0.52" or "surface_variant/0.18"
    local groupBorder = workspace.active and "primary/0.65" or "primary/0.16"
    if urgent then
        groupFill = "error/0.18"
        groupBorder = "error/0.80"
    end

    local groupChildren = {
        ui.row({
            key = "windows",
            align = "center",
            gap = 0,
        }, rowChildren),
    }

    local props = {
        -- Use a new key so hosts that retained the old fixed-width node create
        -- a fresh auto-sized node. Flex then tracks window count and order.
        key = (#windows == 0 and "workspace-empty-" or "workspace-auto-") .. idString(workspace.id),
        direction = "row",
        align = "center",
        justify = "center",
        paddingH = 3,
        paddingV = 2,
        radius = (itemHeight + 4) / 2,
        fill = groupFill,
        border = groupBorder,
        borderWidth = 1,
        opacity = workspace.active and 1.0 or inactiveOpacity,
    }
    if #windows == 0 then
        props.width = itemHeight + 6
    end

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

render = function()
    local output = findOutput(targetOutput())
    if output == nil then
        if config("hide_when_output_missing", false) then
            state.hoveredWindowId = nil
            barWidget.render(ui.row({
                key = "niri-xmb-missing-hidden",
                visible = false,
            }))
            return
        end
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
    local indicatorMode = string.lower(tostring(config("active_indicator_mode", "dot")))
    if indicatorMode ~= "dot" and indicatorMode ~= "underline" and indicatorMode ~= "none" then
        indicatorMode = "dot"
    end
    local hideEmpty = config("hide_empty_workspaces", false)
    local inactiveOpacity = config("group_opacity", 0.72)
    local children = {}

    for _, workspace in ipairs(output.workspaces or {}) do
        local windows = workspace.windows or {}
        if not hideEmpty or allowDrag or workspace.active or #windows > 0 then
            table.insert(children, workspaceNode(
                workspace,
                iconSize,
                allowDrag,
                allowJoin,
                indicatorMode,
                inactiveOpacity
            ))
        end
    end

    if state.dropErrorUntil ~= nil then
        if noctalia.nowMs() < state.dropErrorUntil then
            table.insert(children, ui.glyph({
                key = "drop-error",
                name = "alert-circle",
                size = math.max(12, iconSize - 8),
                color = "error",
            }))
        else
            state.dropErrorUntil = nil
        end
    end

    barWidget.render(ui.row({
        key = "niri-xmb-root",
        align = "center",
        gap = 3,
    }, children))
end

poll = function(force)
    if state.polling then
        return
    end
    state.polling = true
    local started = noctalia.runAsync(command({ "snapshot" }), function(result)
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
        if state.pendingDrop ~= nil then
            local now = noctalia.nowMs()
            if now >= state.pendingDrop.deadline then
                -- The daemon never answered in time; stop trusting the
                -- preview and show whatever canonical state arrived.
                state.pendingDrop = nil
            elseif pendingDropMatches(decoded, state.pendingDrop) then
                state.pendingDrop = nil
            else
                -- A drop is in flight and Niri reports intermediate layout.
                -- Hold the optimistic order until the reply or the deadline;
                -- unrelated updates are delayed by at most that window.
                return
            end
        end
        state.snapshot = decoded
        local dropErrorExpired = state.dropErrorUntil ~= nil
            and noctalia.nowMs() >= state.dropErrorUntil
        if dropErrorExpired then
            state.dropErrorUntil = nil
        end
        if force or dropErrorExpired or decoded.generation ~= state.lastGeneration then
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
    local validatedSteps = finiteNumber(steps)
    if validatedSteps == nil or validatedSteps == 0 then
        return
    end
    local delta = validatedSteps > 0 and 1 or -1
    if axis == "vertical" then
        runAction({ "cycle-workspace", "--output", targetOutput(), "--delta", tostring(delta) })
    elseif axis == "horizontal" then
        -- Wheel left/right moves window focus across the active workspace row.
        runAction({ "cycle-window", "--output", targetOutput(), "--delta", tostring(delta) })
    end
end

function onConfigChanged()
    state.icons = {}
    state.iconRetryAt = {}
    state.lastGeneration = -1
    noctalia.setUpdateInterval(config("poll_interval_ms", 250))
    poll(true)
end

noctalia.setUpdateInterval(config("poll_interval_ms", 250))
render()
poll(true)
