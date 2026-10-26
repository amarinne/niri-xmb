-- Deterministic harness for bar.luau.
--
-- There is no Luau analyzer in this repository, so the plugin is loaded with
-- LuaJIT and driven through mocked Noctalia globals. Run from anywhere:
--
--   luajit ports/noctalia-v5/plugins/niri-xmb/tests/run.lua
--
-- The harness covers stream framing, batching, fallback polling, input
-- intents, and stale callback handling. It does not replace a live session.

local script = debug.getinfo(1, "S").source
if script:sub(1, 1) == "@" then
    script = script:sub(2)
end
local scriptDir = script:match("^(.*)/[^/]+$") or "."
local barPath = scriptDir .. "/../bar.luau"

-- Minimal JSON ---------------------------------------------------------------

local json = {}

local function jsonQuote(value)
    local text = tostring(value)
    text = text:gsub("\\", "\\\\")
    text = text:gsub('"', '\\"')
    text = text:gsub("\n", "\\n")
    text = text:gsub("\r", "\\r")
    text = text:gsub("\t", "\\t")
    return '"' .. text .. '"'
end

local function isArray(value)
    local count = 0
    local maxKey = 0
    for key in pairs(value) do
        if type(key) ~= "number" or key < 1 or key ~= math.floor(key) then
            return false
        end
        if key > maxKey then
            maxKey = key
        end
        count = count + 1
    end
    if count == 0 then
        return true
    end
    return maxKey == count
end

local function encodeValue(value)
    local kind = type(value)
    if kind == "nil" then
        return "null"
    elseif kind == "boolean" then
        return value and "true" or "false"
    elseif kind == "number" then
        if value ~= value or value == math.huge or value == -math.huge then
            error("cannot encode a non-finite number")
        end
        if value == math.floor(value) then
            return string.format("%d", value)
        end
        return tostring(value)
    elseif kind == "string" then
        return jsonQuote(value)
    elseif kind == "table" then
        local parts = {}
        if isArray(value) then
            for index = 1, #value do
                parts[#parts + 1] = encodeValue(value[index])
            end
            return "[" .. table.concat(parts, ",") .. "]"
        end
        for key, item in pairs(value) do
            parts[#parts + 1] = jsonQuote(key) .. ":" .. encodeValue(item)
        end
        table.sort(parts)
        return "{" .. table.concat(parts, ",") .. "}"
    end
    error("cannot encode " .. kind)
end

json.encode = encodeValue

function json.decode(text)
    local position = 1

    local function skipSpace()
        while position <= #text do
            local char = text:sub(position, position)
            if char ~= " " and char ~= "\n" and char ~= "\r" and char ~= "\t" then
                return
            end
            position = position + 1
        end
    end

    local parseValue

    local function parseString()
        position = position + 1
        local parts = {}
        while true do
            local char = text:sub(position, position)
            if char == "" then
                error("unterminated string")
            elseif char == '"' then
                position = position + 1
                return table.concat(parts)
            elseif char == "\\" then
                local escape = text:sub(position + 1, position + 1)
                position = position + 2
                if escape == '"' or escape == "\\" or escape == "/" then
                    parts[#parts + 1] = escape
                elseif escape == "b" then
                    parts[#parts + 1] = "\b"
                elseif escape == "f" then
                    parts[#parts + 1] = "\f"
                elseif escape == "n" then
                    parts[#parts + 1] = "\n"
                elseif escape == "r" then
                    parts[#parts + 1] = "\r"
                elseif escape == "t" then
                    parts[#parts + 1] = "\t"
                elseif escape == "u" then
                    local hex = text:sub(position, position + 3)
                    position = position + 4
                    local code = tonumber(hex, 16)
                    if code == nil then
                        error("bad unicode escape")
                    end
                    if code < 128 then
                        parts[#parts + 1] = string.char(code)
                    elseif code < 2048 then
                        parts[#parts + 1] = string.char(192 + math.floor(code / 64), 128 + code % 64)
                    else
                        parts[#parts + 1] = string.char(
                            224 + math.floor(code / 4096),
                            128 + math.floor(code / 64) % 64,
                            128 + code % 64
                        )
                    end
                else
                    error("bad escape")
                end
            else
                parts[#parts + 1] = char
                position = position + 1
            end
        end
    end

    local function parseNumber()
        local start = position
        if text:sub(position, position) == "-" then
            position = position + 1
        end
        while text:sub(position, position):match("%d") do
            position = position + 1
        end
        if text:sub(position, position) == "." then
            position = position + 1
            while text:sub(position, position):match("%d") do
                position = position + 1
            end
        end
        local exponent = text:sub(position, position)
        if exponent == "e" or exponent == "E" then
            position = position + 1
            local sign = text:sub(position, position)
            if sign == "+" or sign == "-" then
                position = position + 1
            end
            while text:sub(position, position):match("%d") do
                position = position + 1
            end
        end
        local number = tonumber(text:sub(start, position - 1))
        if number == nil then
            error("bad number")
        end
        return number
    end

    local function parseArray()
        position = position + 1
        local result = {}
        skipSpace()
        if text:sub(position, position) == "]" then
            position = position + 1
            return result
        end
        while true do
            result[#result + 1] = parseValue()
            skipSpace()
            local char = text:sub(position, position)
            if char == "]" then
                position = position + 1
                return result
            elseif char == "," then
                position = position + 1
                skipSpace()
            else
                error("expected , or ]")
            end
        end
    end

    local function parseObject()
        position = position + 1
        local result = {}
        skipSpace()
        if text:sub(position, position) == "}" then
            position = position + 1
            return result
        end
        while true do
            if text:sub(position, position) ~= '"' then
                error("expected object key")
            end
            local key = parseString()
            skipSpace()
            if text:sub(position, position) ~= ":" then
                error("expected :")
            end
            position = position + 1
            skipSpace()
            result[key] = parseValue()
            skipSpace()
            local char = text:sub(position, position)
            if char == "}" then
                position = position + 1
                return result
            elseif char == "," then
                position = position + 1
                skipSpace()
            else
                error("expected , or }")
            end
        end
    end

    parseValue = function()
        skipSpace()
        local char = text:sub(position, position)
        if char == "{" then
            return parseObject()
        elseif char == "[" then
            return parseArray()
        elseif char == '"' then
            return parseString()
        elseif char == "t" and text:sub(position, position + 3) == "true" then
            position = position + 4
            return true
        elseif char == "f" and text:sub(position, position + 4) == "false" then
            position = position + 5
            return false
        elseif char == "n" and text:sub(position, position + 3) == "null" then
            position = position + 4
            return nil
        else
            return parseNumber()
        end
    end

    local value = parseValue()
    skipSpace()
    if position <= #text then
        error("trailing data: " .. text:sub(position))
    end
    return value
end

-- Fixtures -------------------------------------------------------------------

local function window(id, title, focused)
    return {
        id = id,
        title = title or ("window " .. tostring(id)),
        app_id = "app" .. tostring(id),
        focused = focused or false,
        urgent = false,
        floating = false,
    }
end

local function snapshot(generation, revision, options)
    options = options or {}
    local activeWorkspace = options.activeWorkspace or 1
    local focusedWindow = options.focusedWindow
    local windowsByWorkspace = options.windowsByWorkspace or {
        [1] = {
            window(10, options.title or "alpha", focusedWindow == 10),
            window(11, "beta", focusedWindow == 11),
        },
        [2] = {
            window(20, "gamma", focusedWindow == 20),
        },
        [3] = {
            window(30, "delta", focusedWindow == 30),
        },
    }
    local workspaceIds = options.workspaceIds or { 1, 2, 3 }
    local workspaces = {}
    for _, workspaceId in ipairs(workspaceIds) do
        workspaces[#workspaces + 1] = {
            id = workspaceId,
            name = tostring(workspaceId),
            active = workspaceId == activeWorkspace,
            active_window = (function()
                local windows = windowsByWorkspace[workspaceId] or {}
                return windows[1] and windows[1].id or nil
            end)(),
            windows = windowsByWorkspace[workspaceId] or {},
        }
    end
    return {
        generation = generation,
        content_revision = revision or 0,
        focused_window = focusedWindow,
        outputs = {
            {
                name = "eDP-1",
                x = 100,
                y = 200,
                width = 1920,
                height = 1080,
                workspaces = workspaces,
            },
        },
    }
end

-- Mock environment -----------------------------------------------------------

local defaults = {
    backend_command = "niri-xmb",
    target_output = "eDP-1",
    poll_interval_ms = 250,
    icon_size = 22,
    allow_drag = true,
    allow_column_join = false,
    preserve_cursor_after_drag = true,
    hide_empty_workspaces = false,
    hide_when_output_missing = false,
    active_indicator_mode = "dot",
    group_opacity = 0.95,
}

local function copyTable(source)
    local result = {}
    for key, value in pairs(source) do
        result[key] = value
    end
    return result
end

local function node(props, children)
    local result = {}
    for key, value in pairs(props or {}) do
        result[key] = value
    end
    result.children = children or {}
    return result
end

local function newEnvironment()
    local env = {
        now = 1000,
        settings = copyTable(defaults),
        renders = 0,
        lastTree = nil,
        logs = {},
        async = {},
        reads = {},
        pendingReads = {},
        intervals = {},
        streamStarts = 0,
        streamStartOk = true,
        streamCallback = nil,
        streamCommand = nil,
        hasRunStream = true,
        deferReads = false,
        mirrorContents = nil,
        mirrorError = nil,
        statusStdout = '{"snapshot_mirror":"/run/niri-xmb.snapshot.json"}\n',
        snapshotStdout = nil,
        instance = "instance-a",
        iconPath = nil,
    }

    local noctalia = {
        nowMs = function()
            return env.now
        end,
        log = function(message)
            env.logs[#env.logs + 1] = tostring(message)
        end,
        getConfig = function(key)
            return env.settings[key]
        end,
        setUpdateInterval = function(interval)
            env.intervals[#env.intervals + 1] = interval
        end,
        runAsync = function(commandText, callback)
            local record = { command = commandText, callback = callback, called = false }
            env.async[#env.async + 1] = record
            if callback ~= nil then
                if commandText:find("'status'", 1, true) then
                    callback({ exitCode = 0, stdout = env.statusStdout, stderr = "" })
                elseif commandText:find("'snapshot'", 1, true) then
                    callback({ exitCode = 0, stdout = env.snapshotStdout or "", stderr = "" })
                end
            end
            return true
        end,
        readFileAsync = function(path, callback)
            local record = { path = path, callback = callback, completed = false }
            env.reads[#env.reads + 1] = record
            if env.deferReads then
                env.pendingReads[#env.pendingReads + 1] = record
            else
                record.completed = true
                callback(env.mirrorContents, env.mirrorError)
            end
            return true
        end,
        appIconPath = function()
            return env.iconPath
        end,
        json = {
            decode = json.decode,
            encode = json.encode,
        },
    }

    if env.hasRunStream then
        noctalia.runStream = function(commandText, callback)
            env.streamStarts = env.streamStarts + 1
            env.streamCommand = commandText
            env.streamCallback = callback
            return env.streamStartOk
        end
    end

    env.noctalia = noctalia
    env.barWidget = {
        outputName = function()
            return "eDP-1"
        end,
        render = function(tree)
            env.renders = env.renders + 1
            env.lastTree = tree
        end,
    }
    env.ui = {
        row = node,
        column = node,
        box = node,
        glyph = node,
        image = node,
        dragSource = node,
        dropZone = node,
    }
    return env
end

local function loadPlugin(env)
    _G.noctalia = env.noctalia
    _G.barWidget = env.barWidget
    _G.ui = env.ui
    _G.update = nil
    _G.onScroll = nil
    _G.onConfigChanged = nil
    local chunk, compileError = loadfile(barPath)
    if chunk == nil then
        error("bar.luau does not compile: " .. tostring(compileError))
    end
    chunk()
    return env
end

-- Tree helpers ---------------------------------------------------------------

local function findNode(tree, key)
    if type(tree) ~= "table" then
        return nil
    end
    if tree.key == key then
        return tree
    end
    for _, child in ipairs(tree.children or {}) do
        local found = findNode(child, key)
        if found ~= nil then
            return found
        end
    end
    return nil
end

local function containsKey(tree, key)
    return findNode(tree, key) ~= nil
end

local function commands(env)
    local result = {}
    for _, record in ipairs(env.async) do
        result[#result + 1] = record.command
    end
    return result
end

local function findCommand(env, needle)
    for index, record in ipairs(env.async) do
        if record.command:find(needle, 1, true) then
            return record, index
        end
    end
    return nil
end

local function lastCommand(env, needle)
    local found = nil
    for _, record in ipairs(env.async) do
        if record.command:find(needle, 1, true) then
            found = record
        end
    end
    return found
end

local function countCommands(env, needle)
    local count = 0
    for _, record in ipairs(env.async) do
        if record.command:find(needle, 1, true) then
            count = count + 1
        end
    end
    return count
end

local function hasLog(env, needle)
    for _, message in ipairs(env.logs) do
        if message:find(needle, 1, true) then
            return true
        end
    end
    return false
end

local function lastInterval(env)
    return env.intervals[#env.intervals]
end

local function emit(env, frame)
    assert(env.streamCallback ~= nil, "no stream callback registered")
    env.streamCallback(json.encode(frame))
end

local function emitHello(env, instance)
    emit(env, { type = "hello", protocol = 1, schema = 1, instance = instance or env.instance })
end

local function emitSnapshot(env, value, instance)
    emit(env, {
        type = "snapshot",
        protocol = 1,
        schema = 1,
        instance = instance or env.instance,
        generation = value.generation,
        snapshot = value,
    })
end

local function emitConnectionLost(env)
    emit(env, {
        type = "connection",
        protocol = 1,
        connected = false,
        reason = "daemon socket closed",
    })
end

local function setMirror(env, value)
    env.mirrorContents = json.encode(value)
end

local function scrollEnvironment()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    update()
    return env
end

local function advance(env, milliseconds)
    env.now = env.now + milliseconds
    update()
end

-- Test runner ----------------------------------------------------------------

local tests = {}
local failures = 0

local function test(name, fn)
    tests[#tests + 1] = { name = name, run = fn }
end

local function assertEqual(actual, expected, message)
    if actual ~= expected then
        error(string.format("%s (expected %s, got %s)", message or "values differ", tostring(expected), tostring(actual)), 2)
    end
end

local function assertTrue(value, message)
    if not value then
        error(message or "expected truthy value", 2)
    end
end

local function assertFalse(value, message)
    if value then
        error(message or "expected falsy value", 2)
    end
end

local function assertRenderDelta(env, before, expected, message)
    assertEqual(env.renders - before, expected, message or "unexpected render count")
end

local function assertCycleDelta(env, needle, delta, message)
    local command = lastCommand(env, needle)
    assertTrue(command ~= nil, message or "scroll command missing")
    assertTrue(
        command.command:find("'" .. tostring(delta) .. "'", 1, true) ~= nil,
        (message or "scroll command") .. " (expected delta " .. tostring(delta) .. ")"
    )
end

-- Tests ----------------------------------------------------------------------

test("stream snapshot applies on the next tick and stops polling", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    assertTrue(containsKey(env.lastTree, "workspace-auto-1"), "baseline workspace is rendered")

    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 2, focusedWindow = 20 }))
    local before = env.renders
    update()
    assertRenderDelta(env, before, 1, "one render for the stream frame")
    assertTrue(containsKey(env.lastTree, "workspace-auto-2"), "new workspace is rendered")
    assertEqual(lastInterval(env), 16, "streaming uses a fast tick")

    local reads = #env.reads
    local processes = #env.async
    for _ = 1, 60 do
        env.now = env.now + 16
        update()
    end
    assertEqual(#env.reads, reads, "healthy streaming does not read the mirror")
    assertEqual(#env.async, processes, "healthy streaming does not spawn a process")
end)

test("event bursts coalesce to the newest frame and one render", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 2, focusedWindow = 20 }))
    update()

    local before = env.renders
    emitSnapshot(env, snapshot(3, 0, { activeWorkspace = 2, focusedWindow = 20, title = "older frame" }))
    emitSnapshot(env, snapshot(4, 0, { activeWorkspace = 2, focusedWindow = 20, title = "newest frame" }))
    update()
    assertRenderDelta(env, before, 1, "two frames render once")
    assertEqual(findNode(env.lastTree, "window-10").tooltip, "newest frame", "newest frame wins")
end)

test("title-only revision is delivered but duplicate frames are not", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(5, 1, { activeWorkspace = 1, focusedWindow = 10, title = "first" }))
    update()
    assertEqual(findNode(env.lastTree, "window-10").tooltip, "first")

    local before = env.renders
    emitSnapshot(env, snapshot(5, 2, { activeWorkspace = 1, focusedWindow = 10, title = "second" }))
    update()
    assertRenderDelta(env, before, 1, "a title-only revision renders")
    assertEqual(findNode(env.lastTree, "window-10").tooltip, "second")

    before = env.renders
    emitSnapshot(env, snapshot(5, 2, { activeWorkspace = 1, focusedWindow = 10, title = "second" }))
    update()
    assertRenderDelta(env, before, 0, "a duplicate frame does not render")
end)

test("older generations cannot replace a newer one", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(8, 0, { activeWorkspace = 2, focusedWindow = 20 }))
    update()

    local before = env.renders
    emitSnapshot(env, snapshot(7, 9, { activeWorkspace = 3, focusedWindow = 30 }))
    update()
    assertRenderDelta(env, before, 0, "older generation is ignored")
    assertTrue(containsKey(env.lastTree, "workspace-auto-2"), "newer state survived")
end)

test("polling fallback works when the stream API is absent", function()
    local env = newEnvironment()
    env.noctalia.runStream = nil
    setMirror(env, snapshot(1, 0, { activeWorkspace = 2, focusedWindow = 20 }))
    loadPlugin(env)
    update()
    assertTrue(containsKey(env.lastTree, "workspace-auto-2"), "mirror state is applied")
    assertEqual(lastInterval(env), 250, "fallback uses the configured interval")
    local reads = #env.reads

    env.now = env.now + 250
    update()
    assertTrue(#env.reads > reads, "fallback keeps reading the mirror")
end)

test("connection loss switches to fallback and recovery switches back", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 2, focusedWindow = 20 }))
    update()
    assertEqual(lastInterval(env), 16)

    emitConnectionLost(env)
    update()
    assertEqual(lastInterval(env), 250, "fallback interval after disconnect")
    local reads = #env.reads
    env.now = env.now + 250
    update()
    assertTrue(#env.reads > reads, "fallback resumed a mirror read")

    setMirror(env, snapshot(3, 0, { activeWorkspace = 3, focusedWindow = 30 }))
    emitHello(env)
    emitSnapshot(env, snapshot(3, 0, { activeWorkspace = 3, focusedWindow = 30 }))
    update()
    assertEqual(lastInterval(env), 16, "stream recovery restores the fast tick")
    assertTrue(containsKey(env.lastTree, "workspace-auto-3"), "recovered snapshot is rendered")
end)

test("stream launches retry with bounded backoff and stop at the cap", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    assertEqual(env.streamStarts, 1, "initial launch")

    env.now = env.now + 1600
    update()
    assertEqual(env.streamStarts, 1, "no retry before backoff")

    env.now = env.now + 2100
    update()
    assertEqual(env.streamStarts, 2, "first retry after the start deadline")

    env.now = env.now + 1600
    update()
    env.now = env.now + 4100
    update()
    assertEqual(env.streamStarts, 3, "second retry")

    env.now = env.now + 1600
    update()
    env.now = env.now + 8100
    update()
    env.now = env.now + 30000
    update()
    assertEqual(env.streamStarts, 3, "launch cap is enforced")
    assertTrue(hasLog(env, "launch limit reached"), "the cap is reported once")
end)

test("the stream end sentinel triggers one bounded relaunch", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 2, focusedWindow = 20 }))
    update()

    emit(env, { type = "stream_end" })
    update()
    assertEqual(lastInterval(env), 250, "fallback while the stream is gone")
    local before = env.streamStarts
    env.now = env.now + 2100
    update()
    assertEqual(env.streamStarts, before + 1, "relaunch after backoff")
end)

test("a new daemon instance resets generation ordering", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env, "instance-a")
    emitSnapshot(env, snapshot(10, 0, { activeWorkspace = 2, focusedWindow = 20 }), "instance-a")
    update()
    assertTrue(containsKey(env.lastTree, "workspace-auto-2"))

    emitHello(env, "instance-b")
    emitSnapshot(env, snapshot(1, 0, { activeWorkspace = 3, focusedWindow = 30 }), "instance-b")
    update()
    assertTrue(containsKey(env.lastTree, "workspace-auto-3"), "lower generation from a new instance applies")

    local before = env.renders
    emitSnapshot(env, snapshot(11, 0, { activeWorkspace = 1, focusedWindow = 10 }), "instance-a")
    update()
    assertRenderDelta(env, before, 0, "a stale instance frame is ignored")
    assertTrue(containsKey(env.lastTree, "workspace-auto-3"))
end)

test("a stale poll result cannot roll back a newer stream frame", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 2, focusedWindow = 20 }))
    update()

    env.deferReads = true
    env.now = env.now + 30000
    update()
    assertTrue(#env.pendingReads > 0, "the resync read is outstanding")

    emitSnapshot(env, snapshot(3, 0, { activeWorkspace = 3, focusedWindow = 30 }))
    update()
    assertTrue(containsKey(env.lastTree, "workspace-auto-3"))

    env.deferReads = false
    local pending = table.remove(env.pendingReads, 1)
    pending.completed = true
    pending.callback(json.encode(snapshot(2, 0, { activeWorkspace = 2, focusedWindow = 20 })), nil)
    update()
    assertTrue(containsKey(env.lastTree, "workspace-auto-3"), "the older poll result was discarded")
end)

test("focus dispatch is immediate and confirms from the next snapshot", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()

    local node11 = findNode(env.lastTree, "window-11")
    assertTrue(node11 ~= nil and node11.onClick ~= nil, "window 11 is clickable")
    node11.onClick()
    local command = findCommand(env, "focus-window")
    assertTrue(command ~= nil, "focus command dispatched immediately")
    assertTrue(command.command:find("'11'", 1, true) ~= nil, "focus target is window 11")

    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 1, focusedWindow = 11 }))
    update()
    env.now = env.now + 5000
    update()
    assertFalse(hasLog(env, "focus was not confirmed"), "confirmed focus does not time out")
end)

test("a contradictory focus snapshot clears the intent", function()
    local env = newEnvironment()
    local base = snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 })
    setMirror(env, base)
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, base)
    update()

    findNode(env.lastTree, "window-11").onClick()
    local windows = {
        window(10, "alpha", false),
        window(11, "beta", false),
        window(12, "external", true),
    }
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 1, focusedWindow = 12, windowsByWorkspace = { [1] = windows } }))
    update()
    env.now = env.now + 5000
    update()
    assertFalse(hasLog(env, "focus was not confirmed"), "contradiction clears the expectation")
end)

test("an unconfirmed focus intent times out and refreshes", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    update()

    findNode(env.lastTree, "window-11").onClick()
    local reads = #env.reads
    env.now = env.now + 2500
    update()
    assertTrue(hasLog(env, "focus was not confirmed"), "timeout is reported")
    assertTrue(#env.reads > reads, "timeout forces a refresh")
end)

test("wheel bursts coalesce, keep the real step count, and confirm", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    update()

    onScroll("vertical", 5, true)
    env.now = env.now + 41
    update()
    local command = findCommand(env, "cycle-workspace")
    assertTrue(command ~= nil, "wheel action dispatched")
    assertTrue(command.command:find("'5'", 1, true) ~= nil, "all five steps are kept")

    emitSnapshot(env, snapshot(3, 0, { activeWorkspace = 2, focusedWindow = 10 }))
    update()
    env.now = env.now + 5000
    update()
    assertFalse(hasLog(env, "workspace cycle was not confirmed"), "an expected target clears the intent")
end)

test("queued wheel targets accumulate until a snapshot arrives", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 2, focusedWindow = 10 }))
    update()

    onScroll("vertical", 1, true)
    env.now = env.now + 41
    update()
    onScroll("vertical", 1, true)
    env.now = env.now + 41
    update()
    assertEqual(countCommands(env, "cycle-workspace"), 2, "each coalesced burst dispatches")

    emitSnapshot(env, snapshot(3, 0, { activeWorkspace = 3, focusedWindow = 10 }))
    update()
    env.now = env.now + 5000
    update()
    assertFalse(hasLog(env, "workspace cycle was not confirmed"), "the accumulated target confirms")
end)

test("an unconfirmed wheel intent times out and refreshes", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    update()

    onScroll("vertical", -1, true)
    env.now = env.now + 41
    update()
    local reads = #env.reads
    env.now = env.now + 2500
    update()
    assertTrue(hasLog(env, "workspace cycle was not confirmed"), "timeout is reported")
    assertTrue(#env.reads > reads, "timeout forces a refresh")
end)

test("touchpad fractions accumulate to one complete step", function()
    local env = scrollEnvironment()
    onScroll("vertical", 0.4, true)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 0, "a sub-step must not scroll")
    onScroll("vertical", 0.4, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 0, "two sub-steps are still below one step")
    onScroll("vertical", 0.2, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1, "the accumulated step dispatches once")
    assertCycleDelta(env, "cycle-workspace", 1)
end)

test("a large fractional frame adds at most one step", function()
    local env = scrollEnvironment()
    onScroll("vertical", 0.9, true)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 0)
    onScroll("vertical", 0.9, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1)
    assertCycleDelta(env, "cycle-workspace", 1)
    onScroll("vertical", 0.9, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1, "overshoot is discarded, not queued")
end)

test("fractional scroll is independent per axis", function()
    local env = scrollEnvironment()
    onScroll("vertical", 0.6, true)
    onScroll("horizontal", 0.6, true)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 0)
    assertEqual(countCommands(env, "cycle-window"), 0)
    onScroll("vertical", 0.4, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1)
    assertEqual(countCommands(env, "cycle-window"), 0)
    onScroll("horizontal", 0.4, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1)
    assertEqual(countCommands(env, "cycle-window"), 1)
    assertCycleDelta(env, "cycle-window", 1)
end)

test("fractional direction reversal clears the residue", function()
    local env = scrollEnvironment()
    onScroll("vertical", 0.6, true)
    onScroll("vertical", -0.6, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 0)
    onScroll("vertical", -0.4, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1)
    assertCycleDelta(env, "cycle-workspace", -1)
end)

test("integer bursts keep their count and their fraction", function()
    local env = scrollEnvironment()
    onScroll("vertical", 2.5, true)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1)
    assertCycleDelta(env, "cycle-workspace", 2)
    onScroll("vertical", 0.5, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 2)
    assertCycleDelta(env, "cycle-workspace", 1)
end)

test("a new gesture or zero start clears the fractional residue", function()
    local env = scrollEnvironment()
    onScroll("vertical", 0.6, false)
    onScroll("vertical", 0.6, true)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 0, "a new gesture must not complete the old residue")
    onScroll("vertical", 0.6, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1)
    onScroll("vertical", 0.6, false)
    onScroll("vertical", 0, true)
    onScroll("vertical", 0.6, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 1, "a zero start must not complete the old residue")
end)

test("config change clears the fractional scroll residue", function()
    local env = scrollEnvironment()
    onScroll("vertical", 0.6, false)
    onConfigChanged()
    onScroll("vertical", 0.6, false)
    advance(env, 41)
    assertEqual(countCommands(env, "cycle-workspace"), 0, "config change starts a fresh residue")
end)

test("drop preview survives a stale reply and the newest reply reconciles", function()
    local env = newEnvironment()
    local base = snapshot(1, 0, {
        activeWorkspace = 1,
        focusedWindow = 10,
        workspaceIds = { 1, 2 },
        windowsByWorkspace = {
            [1] = { window(10, "alpha", true) },
            [2] = { window(20, "beta", false) },
        },
    })
    setMirror(env, base)
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, base)
    update()

    local targetTwo = findNode(env.lastTree, "workspace-auto-2")
    assertTrue(targetTwo ~= nil and targetTwo.onDrop ~= nil, "workspace 2 accepts a drop")
    targetTwo.onDrop("10", "2|0|0.5", 10, 10)
    local first = findCommand(env, "'drop'")
    assertTrue(first ~= nil, "first drop dispatched")
    assertTrue(containsKey(env.lastTree, "window-10"), "optimistic preview exists")

    local targetOne = findNode(env.lastTree, "workspace-auto-1") or findNode(env.lastTree, "workspace-empty-1")
    assertTrue(targetOne ~= nil and targetOne.onDrop ~= nil, "workspace 1 accepts a drop")
    targetOne.onDrop("20", "1|0|0.5", 10, 10)
    local drops = 0
    local second
    for _, record in ipairs(env.async) do
        if record.command:find("'drop'", 1, true) then
            drops = drops + 1
            second = record
        end
    end
    assertEqual(drops, 2, "second drop dispatched")
    local rendersBeforeStale = env.renders

    first.callback({ exitCode = 1, stderr = "error: stale failure" })
    assertRenderDelta(env, rendersBeforeStale, 0, "a stale reply does not repaint")
    assertFalse(hasLog(env, "drop failed"), "a stale reply is not logged as the current drop")

    local reads = #env.reads
    second.callback({ exitCode = 0, stdout = "ok\n" })
    assertTrue(#env.reads > reads, "the newest reply pulls canonical state")
end)

test("a drop preview expires without a reply", function()
    local env = newEnvironment()
    local base = snapshot(1, 0, {
        activeWorkspace = 1,
        focusedWindow = 10,
        workspaceIds = { 1, 2 },
        windowsByWorkspace = {
            [1] = { window(10, "alpha", true) },
            [2] = { window(20, "beta", false) },
        },
    })
    setMirror(env, base)
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, base)
    update()

    local targetTwo = findNode(env.lastTree, "workspace-auto-2")
    assertTrue(targetTwo ~= nil and targetTwo.onDrop ~= nil, "workspace 2 accepts a drop")
    targetTwo.onDrop("10", "2|0|0.5", 10, 10)
    local before = env.renders
    env.now = env.now + 3001
    update()
    assertTrue(hasLog(env, "drop preview expired"), "expiry is reported")
    assertTrue(env.renders > before, "expiry repaints canonical state")
end)

test("malformed stream output falls back instead of crashing", function()
    local env = newEnvironment()
    setMirror(env, snapshot(1, 0, { activeWorkspace = 1, focusedWindow = 10 }))
    loadPlugin(env)
    update()
    emitHello(env)
    emitSnapshot(env, snapshot(2, 0, { activeWorkspace = 2, focusedWindow = 20 }))
    update()

    env.streamCallback("not json")
    update()
    assertTrue(hasLog(env, "non-JSON"), "protocol failure is reported")
    assertEqual(lastInterval(env), 250, "fallback interval restored")
end)

-- Run ------------------------------------------------------------------------

for _, case in ipairs(tests) do
    local ok, err = pcall(case.run)
    if ok then
        print("ok   " .. case.name)
    else
        failures = failures + 1
        print("FAIL " .. case.name)
        print("     " .. tostring(err))
    end
end

print(string.format("%d/%d tests passed", #tests - failures, #tests))
if failures > 0 then
    os.exit(1)
end
