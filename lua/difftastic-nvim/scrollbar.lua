--- Floating scrollbar with change marks for difftastic diff panes.
---
--- Shows a 1-column scrollbar on each diff pane with colored marks indicating
--- where changes are (red = removed, green = added, orange = mixed), plus
--- a cursor position indicator.
local M = {}

local api = vim.api
local ns = api.nvim_create_namespace("difft-scroll")

--- @type table<integer, integer> content winid -> scrollbar winid
local scrollbars = {}

--- @type integer|nil autocmd group id (nil when detached)
local augroup = nil

--- Create or update a 1-column floating scrollbar window anchored to a pane.
--- @param winid integer
--- @return integer?
local function get_or_create_bar(winid)
    if not api.nvim_win_is_valid(winid) then
        return nil
    end

    local height = api.nvim_win_get_height(winid)
    local width = api.nvim_win_get_width(winid)
    if height == 0 or width == 0 then
        return nil
    end

    local cfg = {
        win = winid,
        relative = "win",
        style = "minimal",
        border = "none",
        focusable = false,
        zindex = 45,
        width = 1,
        height = height,
        row = 0,
        col = width - 1,
    }

    local bar_winid = scrollbars[winid]
    if bar_winid and api.nvim_win_is_valid(bar_winid) then
        api.nvim_win_set_config(bar_winid, cfg)
    else
        local buf = api.nvim_create_buf(false, true)
        vim.bo[buf].buftype = "nofile"
        vim.bo[buf].bufhidden = "wipe"
        vim.bo[buf].swapfile = false
        vim.bo[buf].undolevels = -1
        bar_winid = api.nvim_open_win(buf, false, cfg)
        vim.wo[bar_winid].winhighlight = "Normal:Normal"
        vim.wo[bar_winid].winblend = require("difftastic-nvim").config.scrollbar.winblend
        vim.wo[bar_winid].scrollbind = false
        vim.wo[bar_winid].cursorbind = false
        vim.wo[bar_winid].wrap = false
        scrollbars[winid] = bar_winid
    end

    -- Resize scrollbar buffer to match window height
    local bbuf = api.nvim_win_get_buf(bar_winid)
    if api.nvim_buf_line_count(bbuf) ~= height then
        local lines = {}
        for i = 1, height do
            lines[i] = " "
        end
        vim.bo[bbuf].modifiable = true
        api.nvim_buf_set_lines(bbuf, 0, -1, false, lines)
        vim.bo[bbuf].modifiable = false
    end

    return bar_winid
end

--- Map buffer line (0-indexed) to scrollbar row.
--- @param line integer
--- @param line_count integer
--- @param bar_height integer
--- @return integer
local function line_to_bar_pos(line, line_count, bar_height)
    return math.min(math.floor(line / line_count * bar_height), bar_height - 1)
end

--- Scan extmarks for changed lines (excluding fillers), returning set of bar positions.
--- @param bufnr integer Buffer handle
--- @param ext_ns_name string Extmark namespace name
--- @param line_count integer Total lines
--- @param bar_height integer Scrollbar height
--- @return table<integer, boolean> Set of bar positions with changes
local function scan_changes(bufnr, ext_ns_name, line_count, bar_height)
    local ext_ns = api.nvim_create_namespace(ext_ns_name)
    local extmarks = api.nvim_buf_get_extmarks(bufnr, ext_ns, 0, -1, { details = true })
    local positions = {}
    local seen = {}
    for _, em in ipairs(extmarks) do
        local line = em[2]
        if not seen[line] then
            seen[line] = true
            local vt = em[4] and em[4].virt_text
            if not (vt and vt[1] and vt[1][2] == "DifftFiller") then
                positions[line_to_bar_pos(line, line_count, bar_height)] = true
            end
        end
    end
    return positions
end

--- Render scrollbar with 3-color change marks, cursor indicator, and viewport thumb.
--- @param winid integer Content window
--- @param bar_winid integer Scrollbar floating window
--- @param own_marks table<integer, boolean> This pane's change positions
--- @param other_marks table<integer, boolean> Other pane's change positions
--- @param own_hl string Highlight for this side's changes
--- @param is_left boolean Whether this is the left pane
local function render(winid, bar_winid, own_marks, other_marks, own_hl, is_left)
    local bbuf = api.nvim_win_get_buf(bar_winid)
    local height = api.nvim_win_get_height(winid)
    local line_count = api.nvim_buf_line_count(api.nvim_win_get_buf(winid))
    if line_count == 0 or height <= 1 then
        return
    end

    -- Viewport thumb position
    local topline = vim.fn.line("w0", winid)
    local botline = vim.fn.line("w$", winid)
    local thumb_top = math.floor((topline - 1) / line_count * height + 0.5)
    local thumb_bot = math.floor(botline / line_count * height + 0.5)

    -- Cursor position indicator
    local cursor_pos = -1
    if api.nvim_win_is_valid(winid) then
        local ok, cursor = pcall(api.nvim_win_get_cursor, winid)
        if ok then
            cursor_pos = line_to_bar_pos(cursor[1] - 1, line_count, height)
        end
    end

    -- Single pass: cursor > mixed mark > own mark > thumb > background
    api.nvim_buf_clear_namespace(bbuf, ns, 0, -1)
    for i = 0, height - 1 do
        local hl
        local char = " "
        if i == cursor_pos then
            hl = "DifftScrollCursor"
            char = "▬"
        elseif own_marks[i] and other_marks[i] then
            hl = "DifftScrollMixed"
        elseif own_marks[i] then
            hl = own_hl
        elseif i >= thumb_top and i < thumb_bot then
            hl = "DifftScrollBar"
        else
            hl = "DifftScrollBg"
        end
        pcall(api.nvim_buf_set_extmark, bbuf, ns, i, 0, {
            id = i + 1,
            virt_text = { { char, hl } },
            virt_text_pos = "overlay",
        })
    end
    pcall(api.nvim_win_set_cursor, bar_winid, { 1, 0 })
end

local function close_bar(winid)
    local bar_winid = scrollbars[winid]
    if bar_winid and api.nvim_win_is_valid(bar_winid) then
        pcall(api.nvim_win_close, bar_winid, true)
    end
    scrollbars[winid] = nil
end

local function refresh()
    local state = require("difftastic-nvim").state
    if not state.left_win and not state.right_win then
        return
    end

    local active = {}

    -- Scan both panes' changes for cross-referencing
    local left_marks = {}
    local right_marks = {}
    local height = 0

    if state.left_win and api.nvim_win_is_valid(state.left_win) then
        local bufnr = api.nvim_win_get_buf(state.left_win)
        height = api.nvim_win_get_height(state.left_win)
        local line_count = api.nvim_buf_line_count(bufnr)
        if line_count > 0 and height > 1 then
            left_marks = scan_changes(bufnr, "difft-left", line_count, height)
        end
    end
    if state.right_win and api.nvim_win_is_valid(state.right_win) then
        local bufnr = api.nvim_win_get_buf(state.right_win)
        height = api.nvim_win_get_height(state.right_win)
        local line_count = api.nvim_buf_line_count(bufnr)
        if line_count > 0 and height > 1 then
            right_marks = scan_changes(bufnr, "difft-right", line_count, height)
        end
    end

    local panes = {
        { win = state.left_win, own = left_marks, other = right_marks, hl = "DifftScrollRemoved", is_left = true },
        { win = state.right_win, own = right_marks, other = left_marks, hl = "DifftScrollAdded", is_left = false },
    }
    for _, pane in ipairs(panes) do
        if pane.win and api.nvim_win_is_valid(pane.win) then
            local bar = get_or_create_bar(pane.win)
            if bar then
                render(pane.win, bar, pane.own, pane.other, pane.hl, pane.is_left)
                active[pane.win] = true
            end
        end
    end

    for winid in pairs(scrollbars) do
        if not active[winid] then
            close_bar(winid)
        end
    end
end

--- Attach scrollbar autocmds so scrollbars appear and update on diff panes.
function M.attach()
    if augroup then
        return -- already attached
    end
    augroup = api.nvim_create_augroup("DifftScroll", { clear = true })
    api.nvim_create_autocmd({ "BufWinEnter", "WinScrolled", "WinResized", "CursorMoved" }, {
        group = augroup,
        callback = vim.schedule_wrap(refresh),
    })
    -- Initial render
    vim.schedule(refresh)
end

--- Detach scrollbar autocmds and close all scrollbar windows.
function M.detach()
    if augroup then
        api.nvim_del_augroup_by_id(augroup)
        augroup = nil
    end
    for winid in pairs(scrollbars) do
        close_bar(winid)
    end
end

return M
