--- Floating scrollbar with change marks for difftastic diff panes.
---
--- Shows a 1-column scrollbar on each diff pane with colored marks indicating
--- where changes are (red = removed, green = added), like VS Code's minimap.
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

--- Render scrollbar content: viewport thumb + change marks.
--- @param winid integer Content window
--- @param bar_winid integer Scrollbar floating window
--- @param ext_ns_name string Difftastic extmark namespace name
--- @param mark_hl string Highlight group for change marks
local function render(winid, bar_winid, ext_ns_name, mark_hl)
    local bufnr = api.nvim_win_get_buf(winid)
    local bbuf = api.nvim_win_get_buf(bar_winid)
    local height = api.nvim_win_get_height(winid)
    local line_count = api.nvim_buf_line_count(bufnr)
    if line_count == 0 or height <= 1 then
        return
    end

    -- Scan difftastic extmarks for changed lines, deduplicate by bar position
    local ext_ns = api.nvim_create_namespace(ext_ns_name)
    local extmarks = api.nvim_buf_get_extmarks(bufnr, ext_ns, 0, -1, { details = true })
    local mark_positions = {}
    local seen = {}
    for _, em in ipairs(extmarks) do
        local line = em[2]
        if not seen[line] then
            seen[line] = true
            local vt = em[4] and em[4].virt_text
            if not (vt and vt[1] and vt[1][2] == "DifftFiller") then
                mark_positions[line_to_bar_pos(line, line_count, height)] = true
            end
        end
    end

    -- Viewport thumb position
    local topline = vim.fn.line("w0", winid)
    local botline = vim.fn.line("w$", winid)
    local thumb_top = math.floor((topline - 1) / line_count * height + 0.5)
    local thumb_bot = math.floor(botline / line_count * height + 0.5)

    -- Single pass: mark > thumb > background
    api.nvim_buf_clear_namespace(bbuf, ns, 0, -1)
    for i = 0, height - 1 do
        local hl = mark_positions[i] and mark_hl
            or (i >= thumb_top and i < thumb_bot) and "DifftScrollBar"
            or "DifftScrollBg"
        pcall(api.nvim_buf_set_extmark, bbuf, ns, i, 0, {
            id = i + 1,
            virt_text = { { " ", hl } },
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

    local panes = {
        { win = state.left_win, ns = "difft-left", hl = "DifftScrollRemoved" },
        { win = state.right_win, ns = "difft-right", hl = "DifftScrollAdded" },
    }
    for _, pane in ipairs(panes) do
        if pane.win and api.nvim_win_is_valid(pane.win) then
            local bar = get_or_create_bar(pane.win)
            if bar then
                render(pane.win, bar, pane.ns, pane.hl)
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
    api.nvim_create_autocmd({ "BufWinEnter", "WinScrolled", "WinResized" }, {
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
