local STREAM_PROTOCOL = 1
local STREAM_SCHEMA = 1
-- Stream frames are applied on update ticks so an event burst collapses to
-- one render. A fast tick keeps that latency below a display frame; the
-- polling fallback uses the configured interval instead.
local STREAM_TICK_MS = 16
local STREAM_START_TIMEOUT_MS = 1500
local STREAM_SNAPSHOT_TIMEOUT_MS = 3000
local STREAM_RETRY_MIN_MS = 2000
local STREAM_RETRY_MAX_MS = 30000
-- One entry runtime gets a few chances to establish a stream. Every launch
-- that ends without a frame leaves a dead child behind, and Noctalia bounds
-- each host to four live stream children.
local STREAM_MAX_LAUNCHES = 3
-- Noctalia drops a runStream line above 64 KiB, while the daemon allows
-- 256 KiB frames. A low-frequency mirror read recovers from a dropped frame
-- or a stream child that died without an exit callback.
local STREAM_RESYNC_MS = 30000
local FOCUS_INTENT_TIMEOUT_MS = 2000
local CYCLE_INTENT_TIMEOUT_MS = 2000
local CYCLE_INTENT_MAX_MS = 4000
-- The host reports whole logical steps for a wheel and sub-step deltas for a
-- touchpad. Accumulate one host step before dispatching a fractional move;
-- one callback can add at most one such step.
local FRACTIONAL_SCROLL_STEP = 1.0

local state = {
    snapshot = nil,
    -- Last applied snapshot identity. Stream frames carry a daemon instance,
    -- so freshness is comparable within one daemon run. Poll results carry no
    -- instance and are deduplicated by (generation, content_revision).
    lastFrame = nil,
    -- Newest decoded frame not yet applied. Stream and poll callbacks only
    -- replace this slot; update() applies at most one frame and renders once.
    pendingFrame = nil,
    renderPending = false,
    icons = {},
    iconRetryAt = {},
    -- appIconPath can scan desktop entries and the icon theme. Keep those
    -- lookups out of render callbacks and spread them across update ticks so
    -- a window-heavy output cannot exhaust Noctalia's callback CPU budget.
    iconQueue = {},
    iconQueued = {},
    iconQueueHead = 1,
    iconRenderPending = false,
    hoveredWindowId = nil,
    pendingDrop = nil,
    dropErrorUntil = nil,
    -- Inputs that were dispatched but are not yet visible in the authoritative
    -- snapshot. They never drive rendering; they only bound how long the
    -- widget keeps treating its last state as transitional.
    intentSeq = 0,
    pendingFocus = nil,
    pendingCycle = { workspace = nil, window = nil },
    -- Set while the one-off `status` call that learns the mirror path is
    -- outstanding, so ticks in between do not pile up more of them.
    learningMirror = false,
    nextMirrorLearnAt = nil,
    -- Path of the daemon's snapshot mirror, learned once from `status`.
    mirrorPath = nil,
    -- When the in-flight poll must be considered lost. Without this a
    -- callback that never fires leaves `polling` true and freezes the widget.
    polling = false,
    pollDeadline = nil,
    pollSeq = 0,
    lastPollResultSeq = 0,
    nextPollAt = nil,
    -- Native stream state: idle | connecting | awaiting | reconnecting |
    -- healthy | stalled | retrying | unavailable.
    streamMode = "idle",
    streamGeneration = 0,
    streamLaunches = 0,
    streamNextAttemptAt = nil,
    streamFrameDeadline = nil,
    streamInstance = nil,
    streamBackend = nil,
    streamLastFrameAt = nil,
    streamResyncAt = nil,
    tickIntervalMs = nil,
    logged = {},
    -- Coalesced wheel steps waiting to be sent, per axis.
    pendingScroll = { vertical = 0, horizontal = 0 },
    -- Sub-step continuous deltas, kept separate so a touchpad frame cannot
    -- become a whole workspace step by itself.
    scrollRemainder = { vertical = 0, horizontal = 0 },
    scrollFlushAt = nil,
}

local render
local poll
local nextIntentToken
local beginFocusIntent
local beginCycleIntent

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

local function nonNegativeInteger(value)
    local number = finiteNumber(value)
    if number == nil or number < 0 or number ~= math.floor(number) then
        return nil
    end
    return number
end

local function pollInterval()
    return finiteNumber(config("poll_interval_ms", 250)) or 250
end

-- Stream and poll failures can repeat every tick. Log one distinct message,
-- not one per frame, so a broken daemon cannot flood Noctalia's log.
local function logOnce(key, message)
    if state.logged[key] == message then
        return
    end
    state.logged[key] = message
    noctalia.log(message)
end

local function targetOutput()
    local configured = tostring(config("target_output", ""))
    if configured ~= "" then
        return configured
    end
    return barWidget.outputName() or noctalia.focusedOutputName() or ""
end

local function findOutputIn(snapshot, name)
    if snapshot == nil or snapshot.outputs == nil then
        return nil
    end
    for _, output in ipairs(snapshot.outputs) do
        if output.name == name then
            return output
        end
    end
    return nil
end

local function findOutput(name)
    return findOutputIn(state.snapshot, name)
end

local function activeWorkspaceIn(output)
    if output == nil then
        return nil
    end
    for _, workspace in ipairs(output.workspaces or {}) do
        if workspace.active then
            return workspace
        end
    end
    return nil
end

local function activeWorkspaceIdOn(snapshot, outputName)
    local workspace = activeWorkspaceIn(findOutputIn(snapshot, outputName))
    return workspace and workspace.id or nil
end

-- Mirrors step_index() in the daemon, which both cycle policies use.
local function stepIndex(current, delta, count, wrap)
    if count == 0 then
        return 0
    end
    if wrap then
        return ((current + delta) % count + count) % count
    end
    local nextIndex = current + delta
    if nextIndex < 0 then
        return 0
    end
    if nextIndex > count - 1 then
        return count - 1
    end
    return nextIndex
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

local function trim(value)
    return string.match(tostring(value or ""), "^%s*(.-)%s*$") or ""
end

local function runAction(arguments, callback)
    local invocation = command(arguments)
    if callback == nil then
        return noctalia.runAsync(invocation)
    end
    return noctalia.runAsync(invocation, callback)
end

local function actionFailed(result)
    if result == nil or result.exitCode ~= 0 then
        return true
    end
    return string.match(trim(tostring(result.stdout or result.stderr or "")), "^error:") ~= nil
end

nextIntentToken = function()
    state.intentSeq = state.intentSeq + 1
    return state.intentSeq
end

-- Record what the authoritative snapshot must show before an action is
-- considered settled. These intents never render: Niri state stays canonical
-- and the next snapshot wins, while the intent only bounds the transitional
-- period and lets a contradictory snapshot clear it early.
beginFocusIntent = function(kind, id)
    if state.snapshot == nil then
        return nil
    end
    local now = noctalia.nowMs()
    local baselineWindowId = nil
    local baselineWorkspaceId = nil
    local outputName = nil
    if kind == "window" then
        local _, _, _, window = findWindowIn(state.snapshot, id)
        if window == nil or state.snapshot.focused_window == id then
            return nil
        end
        baselineWindowId = state.snapshot.focused_window
    else
        local output, workspace = findWorkspaceIn(state.snapshot, id)
        if workspace == nil or workspace.active then
            return nil
        end
        outputName = output and output.name or nil
        baselineWorkspaceId = activeWorkspaceIdOn(state.snapshot, outputName)
    end
    local token = nextIntentToken()
    state.pendingFocus = {
        token = token,
        kind = kind,
        windowId = kind == "window" and id or nil,
        workspaceId = kind == "workspace" and id or nil,
        output = outputName,
        baselineWindowId = baselineWindowId,
        baselineWorkspaceId = baselineWorkspaceId,
        submittedAt = now,
        deadline = now + FOCUS_INTENT_TIMEOUT_MS,
    }
    return token
end

local function captureWorkspaceBaseline()
    local outputName = targetOutput()
    local output = findOutputIn(state.snapshot, outputName)
    if output == nil then
        return nil
    end
    local ids = {}
    local current = nil
    for index, workspace in ipairs(output.workspaces or {}) do
        ids[index] = workspace.id
        if workspace.active then
            current = index
        end
    end
    if #ids == 0 then
        return nil
    end
    current = current or 1
    return {
        kind = "workspace",
        output = outputName,
        ids = ids,
        current = current,
        baselineId = ids[current],
    }
end

local function captureWindowBaseline()
    local outputName = targetOutput()
    local output = findOutputIn(state.snapshot, outputName)
    local workspace = activeWorkspaceIn(output)
    if workspace == nil then
        return nil
    end
    local ids = {}
    local current = nil
    for index, window in ipairs(workspace.windows or {}) do
        ids[index] = window.id
        if state.snapshot.focused_window == window.id then
            current = index
        end
    end
    if current == nil then
        for index, window in ipairs(workspace.windows or {}) do
            if workspace.active_window == window.id then
                current = index
            end
        end
    end
    if #ids == 0 then
        return nil
    end
    current = current or 1
    return {
        kind = "window",
        output = outputName,
        ids = ids,
        current = current,
        baselineId = ids[current],
    }
end

-- The daemon honours bar.scroll_wrap, which the plugin cannot read. Keep both
-- the wrapping and clamping result as acceptable outcomes.
local function cycleExpectation(baseline, totalDelta)
    local count = #baseline.ids
    if count == 0 then
        return nil
    end
    local wrapIndex = stepIndex(baseline.current - 1, totalDelta, count, true) + 1
    local clampIndex = stepIndex(baseline.current - 1, totalDelta, count, false) + 1
    local expected = {}
    if wrapIndex ~= baseline.current then
        expected[baseline.ids[wrapIndex]] = true
    end
    if clampIndex ~= baseline.current then
        expected[baseline.ids[clampIndex]] = true
    end
    if next(expected) == nil then
        return nil
    end
    return expected
end

beginCycleIntent = function(kind, delta)
    if state.snapshot == nil or delta == 0 then
        return nil
    end
    local now = noctalia.nowMs()
    local intent = state.pendingCycle[kind]
    if intent ~= nil and now < intent.deadline then
        -- A queued wheel burst keeps extending the target it already sent.
        intent.totalDelta = intent.totalDelta + delta
        intent.expected = cycleExpectation(intent.baseline, intent.totalDelta)
        intent.token = nextIntentToken()
        intent.deadline = math.min(intent.submittedAt + CYCLE_INTENT_MAX_MS, now + CYCLE_INTENT_TIMEOUT_MS)
        if intent.expected == nil then
            state.pendingCycle[kind] = nil
            return nil
        end
        return intent.token
    end
    local baseline = kind == "workspace" and captureWorkspaceBaseline() or captureWindowBaseline()
    if baseline == nil then
        return nil
    end
    local expected = cycleExpectation(baseline, delta)
    if expected == nil then
        return nil
    end
    intent = {
        token = nextIntentToken(),
        kind = kind,
        baseline = baseline,
        totalDelta = delta,
        expected = expected,
        submittedAt = now,
        deadline = now + CYCLE_INTENT_TIMEOUT_MS,
    }
    state.pendingCycle[kind] = intent
    return intent.token
end

local function clearFocusIntent(token)
    local intent = state.pendingFocus
    if intent ~= nil and intent.token == token then
        state.pendingFocus = nil
    end
end

local function reportActionFailure(label, detail, token)
    logOnce("action:" .. label, "niri-xmb " .. label .. " failed: " .. detail)
    if token ~= nil then
        clearFocusIntent(token)
        poll(true)
    end
end

local function focusWindow(windowId)
    local validatedId = positiveId(windowId)
    if validatedId == nil then
        noctalia.log("niri-xmb: rejected invalid window ID")
        return
    end
    local token = beginFocusIntent("window", validatedId)
    local started = runAction({ "focus-window", "--window-id", idString(validatedId) }, function(result)
        if actionFailed(result) then
            reportActionFailure("window focus", trim(tostring(result and (result.stdout or result.stderr) or "")), token)
        end
    end)
    if not started then
        reportActionFailure("window focus", "runAsync rejected the command", token)
    end
end

local function focusWorkspace(workspaceId)
    local validatedId = positiveId(workspaceId)
    if validatedId == nil then
        noctalia.log("niri-xmb: rejected invalid workspace ID")
        return
    end
    local token = beginFocusIntent("workspace", validatedId)
    local started = runAction({ "focus-workspace", "--workspace-id", idString(validatedId) }, function(result)
        if actionFailed(result) then
            reportActionFailure("workspace focus", trim(tostring(result and (result.stdout or result.stderr) or "")), token)
        end
    end)
    if not started then
        reportActionFailure("workspace focus", "runAsync rejected the command", token)
    end
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

    if not state.iconQueued[key] then
        state.iconQueued[key] = true
        table.insert(state.iconQueue, {
            candidate = candidate,
            size = size,
            key = key,
        })
    end
    return nil
end

local function resolveNextQueuedIcon()
    while state.iconQueueHead <= #state.iconQueue do
        local request = state.iconQueue[state.iconQueueHead]
        state.iconQueueHead = state.iconQueueHead + 1
        state.iconQueued[request.key] = nil

        local now = noctalia.nowMs()
        local retryAt = state.iconRetryAt[request.key]
        if state.icons[request.key] == nil and (retryAt == nil or now >= retryAt) then
            -- Resolve a larger source than the logical display size. Noctalia
            -- then downsamples instead of enlarging a low-resolution icon.
            local decodeSize = math.max(64, request.size * 2)
            local resolved = noctalia.appIconPath(request.candidate, decodeSize)
            if resolved ~= nil then
                state.icons[request.key] = resolved
                state.iconRetryAt[request.key] = nil
                state.iconRenderPending = true
            else
                -- Desktop entries can finish loading after the first frame.
                state.iconRetryAt[request.key] = now + 5000
            end
            return
        end
    end

    state.iconQueue = {}
    state.iconQueueHead = 1
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
        token = nextIntentToken(),
        sourceWindowId = sourceWindowId,
        workspaceId = workspaceId,
        targetWindowId = targetWindowId,
        fraction = fraction,
        submittedAt = noctalia.nowMs(),
        deadline = noctalia.nowMs() + 3000,
    }
    -- The local preview is not a frame. Drop any queued pre-drop state so the
    -- next authoritative frame is judged against the preview, not the past.
    state.pendingFrame = nil
    state.lastFrame = nil
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

    local previewed = applyOptimisticDrop(sourceWindowId, workspaceId, targetWindowId, fraction)
    local token = previewed and state.pendingDrop.token or nil

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

    local function finishDrop(dropToken, result)
        local pending = state.pendingDrop
        if pending ~= nil and dropToken ~= pending.token then
            -- A newer drag owns the preview now. Its reply is the one that
            -- gets to clear or roll back canonical state.
            return
        end
        local response = trim(tostring(result and (result.stdout or result.stderr) or ""))
        -- The daemon answers only after the move is confirmed, the action
        -- fails, or confirmation times out. Either way this reply ends the
        -- optimistic preview and pulls canonical state immediately.
        local pendingAge = nil
        if pending ~= nil and finiteNumber(pending.submittedAt) ~= nil then
            pendingAge = math.max(0, math.floor(noctalia.nowMs() - pending.submittedAt))
        end
        state.pendingDrop = nil
        state.pendingFrame = nil
        state.lastFrame = nil
        if actionFailed(result) then
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
    end

    if not runAction(arguments, function(result)
        finishDrop(token, result)
    end) then
        finishDrop(token, nil)
    end
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
        -- Keep a thin full-height insertion marker in the idle layout, then
        -- let the host animate it to the dragged icon's width while it is
        -- targeted, so neighbouring icons slide aside as a drop preview.
        -- The marker's hit slop makes the insertion point reachable without
        -- idle gaps.
        width = 4,
        height = height,
        accepts = { "niri-window" },
        value = idString(workspaceId) .. "|" .. idString(targetWindowId) .. "|" .. tostring(fraction),
        onDrop = onDrop,
        hitSlop = slop,
        expandOnDrag = true,
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
        -- Let the animated insertion marker reserve the space while the
        -- source ghost follows the pointer.
        props.liftFromLayout = true
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

-- Frame identity -----------------------------------------------------------

-- Stream frames from one daemon run are ordered by generation and then by
-- content_revision, because a title-only change keeps the generation and
-- moves the revision. Poll frames have no instance, so they are deduplicated
-- by the tuple alone; a poll that started before a newer stream frame is
-- discarded by request time in poll().
local function frameIsNewer(frame, reference)
    if reference == nil then
        return true
    end
    if frame.instance ~= nil and reference.instance ~= nil then
        if frame.instance ~= reference.instance then
            return true
        end
        if frame.generation ~= reference.generation then
            return frame.generation > reference.generation
        end
        return frame.contentRevision > reference.contentRevision
    end
    if frame.instance ~= nil or reference.instance ~= nil then
        return true
    end
    return frame.generation ~= reference.generation
        or frame.contentRevision ~= reference.contentRevision
end

local function queueFrame(frame)
    local reference = state.pendingFrame or state.lastFrame
    if not frameIsNewer(frame, reference) then
        return false
    end
    state.pendingFrame = frame
    return true
end

local function snapshotFromPoll(decoded)
    if type(decoded) ~= "table" then
        return nil, "not an object"
    end
    local generation = nonNegativeInteger(decoded.generation)
    if generation == nil then
        return nil, "missing generation"
    end
    return {
        instance = nil,
        generation = generation,
        contentRevision = nonNegativeInteger(decoded.content_revision) or 0,
        snapshot = decoded,
        source = "poll",
    }
end

-- Native stream transport --------------------------------------------------

local STREAM_END_FRAME = '{"type":"stream_end"}'

local function streamCommand()
    -- runStream has no exit callback in this API level. The trailing printf
    -- runs only if the subscribe client itself exits, which turns an
    -- otherwise silent death into one parsable line.
    return command({ "subscribe" }) .. "; printf '%s\\n' " .. shellQuote(STREAM_END_FRAME)
end

local function streamRetryDelay()
    local exponent = math.max(0, state.streamLaunches - 1)
    local delay = STREAM_RETRY_MIN_MS * (2 ^ exponent)
    if delay > STREAM_RETRY_MAX_MS then
        delay = STREAM_RETRY_MAX_MS
    end
    return delay
end

local function markStreamHealthy(now)
    state.streamMode = "healthy"
    state.streamNextAttemptAt = nil
    state.streamFrameDeadline = nil
    state.streamResyncAt = now + STREAM_RESYNC_MS
    state.nextPollAt = nil
end

local function handleStreamLine(line)
    if type(line) ~= "string" then
        return
    end
    local stripped = trim(line)
    if stripped == "" then
        return
    end
    local ok, decoded = pcall(noctalia.json.decode, stripped)
    if not ok or type(decoded) ~= "table" then
        state.streamMode = "unavailable"
        state.nextPollAt = noctalia.nowMs()
        logOnce("stream-protocol", "niri-xmb: stream emitted a non-JSON line; using snapshot polling")
        return
    end

    local now = noctalia.nowMs()
    local kind = decoded.type

    if kind == "hello" then
        local protocol = nonNegativeInteger(decoded.protocol)
        local schema = nonNegativeInteger(decoded.schema)
        local instance = decoded.instance
        if protocol ~= STREAM_PROTOCOL or schema ~= STREAM_SCHEMA
            or type(instance) ~= "string" or instance == "" then
            state.streamMode = "unavailable"
            state.nextPollAt = now
            logOnce("stream-hello", string.format(
                "niri-xmb: unsupported stream hello (protocol=%s schema=%s); using snapshot polling",
                tostring(decoded.protocol),
                tostring(decoded.schema)
            ))
            return
        end
        if state.streamInstance ~= nil and state.streamInstance ~= instance
            and state.pendingFrame ~= nil and state.pendingFrame.instance ~= nil
            and state.pendingFrame.instance ~= instance then
            -- A queued frame from the replaced daemon must not be applied
            -- after the new instance's hello.
            state.pendingFrame = nil
        end
        state.streamInstance = instance
        state.streamMode = "awaiting"
        state.streamFrameDeadline = now + STREAM_SNAPSHOT_TIMEOUT_MS
        return
    end

    if kind == "snapshot" then
        local protocol = nonNegativeInteger(decoded.protocol)
        local schema = nonNegativeInteger(decoded.schema)
        local instance = decoded.instance
        local generation = nonNegativeInteger(decoded.generation)
        local body = decoded.snapshot
        if protocol ~= STREAM_PROTOCOL or schema ~= STREAM_SCHEMA
            or type(instance) ~= "string" or instance == ""
            or generation == nil or type(body) ~= "table" then
            logOnce("stream-frame", "niri-xmb: rejected malformed stream snapshot frame")
            return
        end
        if state.streamInstance ~= nil and instance ~= state.streamInstance then
            -- A replaced daemon's buffered frame must not overwrite the new
            -- instance's state.
            return
        end
        if nonNegativeInteger(body.generation) ~= generation then
            logOnce("stream-generation", "niri-xmb: rejected stream frame with a mismatched snapshot generation")
            return
        end
        state.streamInstance = instance
        state.streamLastFrameAt = now
        markStreamHealthy(now)
        queueFrame({
            instance = instance,
            generation = generation,
            contentRevision = nonNegativeInteger(body.content_revision) or 0,
            snapshot = body,
            source = "stream",
        })
        return
    end

    if kind == "connection" then
        state.streamMode = "reconnecting"
        state.streamFrameDeadline = nil
        state.nextPollAt = now
        logOnce(
            "stream-connection",
            "niri-xmb: state stream disconnected (" .. tostring(decoded.reason or "daemon socket closed")
                .. "); polling fallback active"
        )
        return
    end

    if kind == "error" then
        local code = tostring(decoded.code or "unknown")
        local message = tostring(decoded.message or "")
        if decoded.retryable == true then
            -- The subscribe client owns reconnecting for this connection.
            state.streamMode = "reconnecting"
            state.nextPollAt = now
            logOnce("stream-error:" .. code, "niri-xmb: stream busy (" .. code .. "); polling fallback active")
        else
            state.streamMode = "unavailable"
            state.streamFrameDeadline = nil
            state.nextPollAt = now
            logOnce(
                "stream-error:" .. code,
                "niri-xmb: stream unavailable (" .. code .. ": " .. message .. "); polling fallback active"
            )
        end
        return
    end

    if kind == "stream_end" then
        state.streamInstance = nil
        state.streamMode = "retrying"
        state.streamFrameDeadline = nil
        state.nextPollAt = now
        state.streamNextAttemptAt = now + streamRetryDelay()
        logOnce("stream-end", "niri-xmb: state stream process ended; polling fallback active")
        return
    end
    -- Unknown frame types are ignored so a newer daemon can add fields.
end

local function startStream()
    if type(noctalia.runStream) ~= "function" then
        state.streamMode = "unavailable"
        logOnce("stream-api", "niri-xmb: Noctalia has no runStream; using snapshot polling")
        return false
    end
    if state.streamLaunches >= STREAM_MAX_LAUNCHES then
        state.streamMode = "unavailable"
        logOnce("stream-launch-limit", "niri-xmb: stream launch limit reached; using snapshot polling")
        return false
    end
    if state.streamMode == "healthy" or state.streamMode == "connecting" or state.streamMode == "awaiting" then
        return false
    end

    local now = noctalia.nowMs()
    state.streamLaunches = state.streamLaunches + 1
    state.streamGeneration = state.streamGeneration + 1
    local generation = state.streamGeneration
    state.streamBackend = backend()
    state.streamInstance = nil
    state.streamMode = "connecting"
    state.streamNextAttemptAt = nil
    state.streamFrameDeadline = now + STREAM_START_TIMEOUT_MS
    state.nextPollAt = now

    local started = noctalia.runStream(streamCommand(), function(line)
        if state.streamGeneration ~= generation then
            return
        end
        local ok, err = pcall(handleStreamLine, line)
        if not ok then
            logOnce("stream-callback", "niri-xmb: stream callback failed: " .. tostring(err))
        end
    end)

    if started then
        logOnce("stream-start", "niri-xmb: subscribed to the daemon state stream")
        return true
    end
    state.streamMode = "retrying"
    state.streamNextAttemptAt = now + streamRetryDelay()
    logOnce("stream-start-failed", "niri-xmb: runStream rejected the subscription; using snapshot polling")
    return false
end

-- Pending input intents ----------------------------------------------------

local function resolveFocusIntent(snapshot, now)
    local intent = state.pendingFocus
    if intent == nil then
        return
    end
    if intent.kind == "window" then
        if snapshot.focused_window == intent.windowId then
            state.pendingFocus = nil
            return
        end
        local _, _, _, window = findWindowIn(snapshot, intent.windowId)
        if window == nil then
            state.pendingFocus = nil
            return
        end
        if snapshot.focused_window ~= nil and snapshot.focused_window ~= intent.baselineWindowId then
            state.pendingFocus = nil
            return
        end
    else
        local _, workspace = findWorkspaceIn(snapshot, intent.workspaceId)
        if workspace == nil then
            state.pendingFocus = nil
            return
        end
        if workspace.active then
            state.pendingFocus = nil
            return
        end
        local active = activeWorkspaceIdOn(snapshot, intent.output)
        if active ~= nil and active ~= intent.baselineWorkspaceId then
            state.pendingFocus = nil
            return
        end
    end
    if now >= intent.deadline then
        state.pendingFocus = nil
        logOnce("focus-timeout", "niri-xmb: focus was not confirmed; refreshing state")
        poll(true)
    end
end

local function resolveCycleIntent(kind, snapshot, now)
    local intent = state.pendingCycle[kind]
    if intent == nil then
        return
    end
    local actual
    if intent.baseline.kind == "workspace" then
        actual = activeWorkspaceIdOn(snapshot, intent.baseline.output)
    else
        actual = snapshot.focused_window
    end
    if actual ~= nil and intent.expected[actual] then
        state.pendingCycle[kind] = nil
        return
    end
    if actual ~= nil and actual ~= intent.baseline.baselineId then
        -- Authoritative state moved somewhere the queued target did not
        -- predict, so waiting for the prediction would only delay the truth.
        state.pendingCycle[kind] = nil
        return
    end
    if now >= intent.deadline then
        state.pendingCycle[kind] = nil
        logOnce("cycle-timeout:" .. kind, "niri-xmb: " .. kind .. " cycle was not confirmed; refreshing state")
        poll(true)
    end
end

local function expireIntents(now)
    local drop = state.pendingDrop
    if drop ~= nil and now >= drop.deadline then
        state.pendingDrop = nil
        state.lastFrame = nil
        state.renderPending = true
        logOnce("drop-timeout", "niri-xmb: drop preview expired before confirmation; refreshing state")
        poll(true)
    end
    local focus = state.pendingFocus
    if focus ~= nil and now >= focus.deadline then
        state.pendingFocus = nil
        logOnce("focus-timeout", "niri-xmb: focus was not confirmed; refreshing state")
        poll(true)
    end
    for _, kind in ipairs({ "workspace", "window" }) do
        local intent = state.pendingCycle[kind]
        if intent ~= nil and now >= intent.deadline then
            state.pendingCycle[kind] = nil
            logOnce("cycle-timeout:" .. kind, "niri-xmb: " .. kind .. " cycle was not confirmed; refreshing state")
            poll(true)
        end
    end
end

-- Applying state -----------------------------------------------------------

-- Everything that happens once a snapshot has been decoded, whether it came
-- from the stream or from the polling fallback.
local function applySnapshot(frame)
    local decoded = frame.snapshot
    if state.pendingDrop ~= nil then
        local now = noctalia.nowMs()
        if now >= state.pendingDrop.deadline then
            state.pendingDrop = nil
        elseif pendingDropMatches(decoded, state.pendingDrop) then
            state.pendingDrop = nil
        else
            return false
        end
    end
    state.snapshot = decoded
    local now = noctalia.nowMs()
    resolveFocusIntent(decoded, now)
    resolveCycleIntent("workspace", decoded, now)
    resolveCycleIntent("window", decoded, now)
    if state.dropErrorUntil ~= nil and now >= state.dropErrorUntil then
        state.dropErrorUntil = nil
    end
    state.renderPending = true
    return true
end

local function applyPendingFrame()
    local frame = state.pendingFrame
    if frame == nil then
        return false
    end
    state.pendingFrame = nil
    if not applySnapshot(frame) then
        return false
    end
    state.lastFrame = {
        instance = frame.instance,
        generation = frame.generation,
        contentRevision = frame.contentRevision,
    }
    return true
end

-- Polling fallback ---------------------------------------------------------

-- One `status` call teaches the widget where the snapshot mirror lives.
-- After that every tick is a file read: no process, no socket.
local function learnMirrorPath(now)
    if state.mirrorPath ~= nil or state.learningMirror then
        return
    end
    if state.nextMirrorLearnAt ~= nil and now < state.nextMirrorLearnAt then
        return
    end
    state.learningMirror = true
    local started = noctalia.runAsync(command({ "status" }), function(result)
        state.learningMirror = false
        if result.exitCode ~= 0 then
            state.nextMirrorLearnAt = noctalia.nowMs() + 10000
            return
        end
        local decoded = noctalia.json.decode(result.stdout or "")
        if type(decoded) == "table" and type(decoded.snapshot_mirror) == "string" then
            state.mirrorPath = decoded.snapshot_mirror
            state.nextMirrorLearnAt = nil
        else
            state.nextMirrorLearnAt = noctalia.nowMs() + 10000
        end
    end)
    if not started then
        state.learningMirror = false
        state.nextMirrorLearnAt = now + 10000
    end
end

poll = function(force)
    local now = noctalia.nowMs()
    if not force and state.streamMode == "healthy" then
        return
    end
    if state.polling then
        if state.pollDeadline ~= nil and now >= state.pollDeadline then
            state.polling = false
            state.pollDeadline = nil
        else
            return
        end
    end

    local interval = pollInterval()
    if not force and state.nextPollAt ~= nil and now < state.nextPollAt then
        return
    end
    state.nextPollAt = now + interval
    learnMirrorPath(now)

    local function beginPoll()
        state.polling = true
        state.pollDeadline = now + math.max(interval * 4, 1000)
    end
    local function endPoll()
        state.polling = false
        state.pollDeadline = nil
        state.nextPollAt = noctalia.nowMs() + interval
    end

    local function handleDecoded(decoded, errorMessage)
        local frame, reason = snapshotFromPoll(decoded)
        if frame == nil then
            logOnce("poll-json", "niri-xmb snapshot JSON failed: " .. tostring(errorMessage or reason or "invalid JSON"))
            return
        end
        queueFrame(frame)
    end

    local requestedAt = now
    state.pollSeq = state.pollSeq + 1
    local sequence = state.pollSeq

    local function acceptPoll()
        if sequence < state.lastPollResultSeq then
            return false
        end
        state.lastPollResultSeq = sequence
        if state.streamMode == "healthy" and requestedAt < (state.streamLastFrameAt or 0) then
            -- A stream frame arrived while this read was outstanding.
            return false
        end
        return true
    end

    local function pollByProcess()
        beginPoll()
        local started = noctalia.runAsync(command({ "snapshot" }), function(result)
            endPoll()
            if not acceptPoll() then
                return
            end
            if result.exitCode ~= 0 then
                logOnce(
                    "poll-process",
                    "niri-xmb snapshot failed: " .. tostring(result.stderr or result.stdout or "snapshot failed")
                )
                return
            end
            local decoded, errorMessage = noctalia.json.decode(result.stdout or "")
            handleDecoded(decoded, errorMessage)
        end)
        if not started then
            endPoll()
        end
    end

    if state.mirrorPath ~= nil and type(noctalia.readFileAsync) == "function" then
        beginPoll()
        local started = noctalia.readFileAsync(state.mirrorPath, function(contents, err)
            endPoll()
            if err ~= nil or contents == nil then
                state.mirrorPath = nil
                state.nextMirrorLearnAt = noctalia.nowMs() + 10000
                pollByProcess()
                return
            end
            if not acceptPoll() then
                return
            end
            local decoded, errorMessage = noctalia.json.decode(contents)
            handleDecoded(decoded, errorMessage)
        end)
        if started then
            return
        end
        endPoll()
    end

    pollByProcess()
end

local function desiredTickInterval()
    local mode = state.streamMode
    if mode == "healthy" or mode == "connecting" or mode == "awaiting" then
        return STREAM_TICK_MS
    end
    return pollInterval()
end

local function applyTickInterval()
    local desired = desiredTickInterval()
    if state.tickIntervalMs == desired then
        return
    end
    state.tickIntervalMs = desired
    noctalia.setUpdateInterval(desired)
end

local function manageStream(now)
    local mode = state.streamMode
    if mode == "connecting" then
        if state.streamFrameDeadline ~= nil and now >= state.streamFrameDeadline then
            state.streamMode = "retrying"
            state.streamFrameDeadline = nil
            state.streamNextAttemptAt = now + streamRetryDelay()
            logOnce("stream-silent", "niri-xmb: stream produced no frames; retrying with backoff")
        end
    elseif mode == "awaiting" then
        if state.streamFrameDeadline ~= nil and now >= state.streamFrameDeadline then
            state.streamMode = "stalled"
            state.streamFrameDeadline = nil
            logOnce("stream-no-snapshot", "niri-xmb: stream connected without a snapshot; polling fallback active")
        end
    elseif mode == "retrying" then
        if state.streamNextAttemptAt == nil or now >= state.streamNextAttemptAt then
            startStream()
        end
    end
end

-- A runStream line above Noctalia's 64 KiB cap is dropped silently, and the
-- stream process has no exit callback. A cheap mirror read recovers both
-- cases without restoring per-tick process polling.
local function maybeResyncStream(now)
    if state.streamMode ~= "healthy" then
        state.streamResyncAt = nil
        return
    end
    if state.streamResyncAt == nil then
        state.streamResyncAt = now + STREAM_RESYNC_MS
        return
    end
    if now < state.streamResyncAt then
        return
    end
    state.streamResyncAt = now + STREAM_RESYNC_MS
    if state.mirrorPath ~= nil then
        poll(true)
    end
end

local function dispatchCycle(kind, delta)
    -- Register the expectation before the process starts, against the state
    -- the daemon will also see. A failure callback invalidates the queued
    -- target even if a newer burst has already moved the token.
    beginCycleIntent(kind, delta)
    local arguments
    if kind == "workspace" then
        arguments = { "cycle-workspace", "--output", targetOutput(), "--delta", idString(delta) }
    else
        arguments = { "cycle-window", "--output", targetOutput(), "--delta", idString(delta) }
    end
    local started = runAction(arguments, function(result)
        if actionFailed(result) then
            state.pendingCycle[kind] = nil
            logOnce(
                "cycle-failure:" .. kind,
                "niri-xmb " .. kind .. " cycle failed: "
                    .. trim(tostring(result and (result.stdout or result.stderr) or ""))
            )
            poll(true)
        end
    end)
    if not started then
        state.pendingCycle[kind] = nil
        logOnce("cycle-dispatch:" .. kind, "niri-xmb " .. kind .. " cycle dispatch was rejected")
        poll(true)
    end
end

local function flushScroll()
    if state.scrollFlushAt == nil or noctalia.nowMs() < state.scrollFlushAt then
        return
    end
    state.scrollFlushAt = nil
    local vertical = state.pendingScroll.vertical
    local horizontal = state.pendingScroll.horizontal
    state.pendingScroll.vertical = 0
    state.pendingScroll.horizontal = 0
    if vertical ~= 0 then
        dispatchCycle("workspace", vertical)
    end
    if horizontal ~= 0 then
        dispatchCycle("window", horizontal)
    end
end

function update()
    local now = noctalia.nowMs()
    flushScroll()
    applyTickInterval()
    manageStream(now)
    expireIntents(now)
    poll(false)
    maybeResyncStream(now)
    applyPendingFrame()

    -- State frames, icon resolution, and presentation expiry share one render
    -- per tick. Pointer and keyboard callbacks still render immediately.
    if state.iconRenderPending then
        state.iconRenderPending = false
        state.renderPending = false
        render()
        return
    end
    if state.renderPending then
        state.renderPending = false
        render()
        return
    end
    resolveNextQueuedIcon()
    if state.iconRenderPending then
        state.iconRenderPending = false
        render()
    end
end

function onScroll(axis, steps, startsGesture)
    -- The host reports complete signed logical steps for a wheel and fractional
    -- deltas for a touchpad. Keep integer bursts intact, but require a
    -- fractional gesture to accumulate a full host step before it queues a
    -- workspace/window move. The pending totals stay separate by axis and
    -- flush through one intent per axis.
    if axis ~= "vertical" and axis ~= "horizontal" then
        return
    end
    local validatedSteps = finiteNumber(steps)
    if validatedSteps == nil then
        return
    end
    if startsGesture == true then
        state.scrollRemainder[axis] = 0
    end
    if validatedSteps == 0 then
        return
    end

    local whole = 0
    local fraction = validatedSteps
    if validatedSteps >= 1 then
        whole = math.floor(validatedSteps)
        fraction = validatedSteps - whole
    elseif validatedSteps <= -1 then
        whole = math.ceil(validatedSteps)
        fraction = validatedSteps - whole
    end

    local dispatch = whole
    if fraction ~= 0 then
        local remainder = state.scrollRemainder[axis]
        if remainder ~= 0 and remainder * fraction < 0 then
            -- A direction reversal starts a fresh gesture residue.
            remainder = 0
        end
        remainder = remainder + fraction
        if math.abs(remainder) >= FRACTIONAL_SCROLL_STEP then
            -- One callback adds at most one fractional step. Discard the
            -- overshoot so a large frame cannot queue several moves.
            dispatch = dispatch + (remainder > 0 and 1 or -1)
            remainder = 0
        end
        state.scrollRemainder[axis] = remainder
    elseif whole ~= 0 then
        -- A pure whole-step event is a discrete wheel report. Do not carry a
        -- stale touchpad sub-step into a different device gesture.
        state.scrollRemainder[axis] = 0
    end

    if dispatch == 0 then
        return
    end
    state.pendingScroll[axis] = state.pendingScroll[axis] + dispatch
    state.scrollFlushAt = noctalia.nowMs() + 40
end

function onConfigChanged()
    state.icons = {}
    state.iconRetryAt = {}
    state.iconQueue = {}
    state.iconQueued = {}
    state.iconQueueHead = 1
    state.iconRenderPending = false
    state.lastFrame = nil
    state.pendingFrame = nil
    state.pendingFocus = nil
    state.pendingCycle.workspace = nil
    state.pendingCycle.window = nil
    state.pendingScroll.vertical = 0
    state.pendingScroll.horizontal = 0
    state.scrollRemainder.vertical = 0
    state.scrollRemainder.horizontal = 0
    state.scrollFlushAt = nil
    state.nextPollAt = nil

    -- The stream command captured the old backend. There is no stop handle in
    -- this API level, so detach the old callback and let the bounded launch
    -- path establish a new subscription.
    if state.streamBackend ~= nil and state.streamBackend ~= backend() then
        state.streamGeneration = state.streamGeneration + 1
        state.streamMode = "idle"
        state.streamInstance = nil
        state.streamLaunches = 0
        state.streamNextAttemptAt = nil
        startStream()
    end

    applyTickInterval()
    render()
    poll(true)
end

applyTickInterval()
render()
poll(true)
startStream()
applyTickInterval()
