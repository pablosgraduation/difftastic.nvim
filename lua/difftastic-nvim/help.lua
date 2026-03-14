--- Help overlay showing keybinding reference.
local M = {}

local api = vim.api

--- @type integer|nil floating window handle
local help_win = nil
--- @type integer|nil floating buffer handle
local help_buf = nil

--- Build the help text lines using the current keymap config.
--- @return string[]
local function build_lines()
    local keys = require("difftastic-nvim").config.keymaps
    local lines = {
        "  Difftastic Keybindings",
        "",
        "  Navigation",
        string.format("  %-16s Next file", keys.next_file or "]f"),
        string.format("  %-16s Previous file", keys.prev_file or "[f"),
        string.format("  %-16s Next hunk", keys.next_hunk or "]c"),
        string.format("  %-16s Previous hunk", keys.prev_hunk or "[c"),
        "",
        "  Actions",
        string.format("  %-16s Select file from tree", keys.select or "<CR>"),
        string.format("  %-16s Go to file in editor", keys.goto_file or "gf"),
        string.format("  %-16s Focus tree panel", keys.focus_tree or "<Tab>"),
        string.format("  %-16s Focus diff panel", keys.focus_diff or "<Tab>"),
        string.format("  %-16s Close diff view", keys.close or "q"),
        string.format("  %-16s Toggle this help", "?"),
        "",
        "  Vim builtins work: /, n, gg, G, Ctrl-d, Ctrl-u",
        "",
        "  Press ? or q to close",
    }
    return lines
end

--- Toggle the help overlay.
--- @param state table Plugin state (used to anchor the overlay)
function M.toggle(state)
    if help_win and api.nvim_win_is_valid(help_win) then
        api.nvim_win_close(help_win, true)
        help_win = nil
        help_buf = nil
        return
    end

    -- Find a valid anchor window for the overlay
    local anchor = state.right_win or state.left_win
    if not anchor or not api.nvim_win_is_valid(anchor) then
        anchor = 0
    end

    local lines = build_lines()
    local width = 48
    local height = #lines

    help_buf = api.nvim_create_buf(false, true)
    vim.bo[help_buf].buftype = "nofile"
    vim.bo[help_buf].bufhidden = "wipe"
    vim.bo[help_buf].swapfile = false

    api.nvim_buf_set_lines(help_buf, 0, -1, false, lines)
    vim.bo[help_buf].modifiable = false

    -- Center in the editor
    local ui_width = vim.o.columns
    local ui_height = vim.o.lines
    local row = math.max(0, math.floor((ui_height - height) / 2))
    local col = math.max(0, math.floor((ui_width - width) / 2))

    help_win = api.nvim_open_win(help_buf, true, {
        relative = "editor",
        width = width,
        height = height,
        row = row,
        col = col,
        style = "minimal",
        border = "rounded",
        title = " Help ",
        title_pos = "center",
        zindex = 50,
    })

    -- Highlight the header
    local ns = api.nvim_create_namespace("difft-help")
    api.nvim_buf_add_highlight(help_buf, ns, "Title", 0, 0, -1)
    for i, line in ipairs(lines) do
        if line:match("^  %S") and not line:match("%-") and (
            line:match("Navigation") or line:match("Actions") or line:match("Vim builtins")
        ) then
            api.nvim_buf_add_highlight(help_buf, ns, "Type", i - 1, 0, -1)
        end
    end

    -- Close on q, Esc, or ?
    local function close()
        if help_win and api.nvim_win_is_valid(help_win) then
            api.nvim_win_close(help_win, true)
        end
        help_win = nil
        help_buf = nil
    end

    vim.keymap.set("n", "q", close, { buffer = help_buf, nowait = true })
    vim.keymap.set("n", "<Esc>", close, { buffer = help_buf, nowait = true })
    vim.keymap.set("n", "?", close, { buffer = help_buf, nowait = true })

    -- Prevent cursor movement from scrolling the diff buffers behind the overlay
    local nop = "<Nop>"
    for _, key in ipairs({ "j", "k", "<Up>", "<Down>", "<C-d>", "<C-u>", "<C-f>", "<C-b>", "gg", "G" }) do
        vim.keymap.set("n", key, nop, { buffer = help_buf, nowait = true })
    end
end

return M
