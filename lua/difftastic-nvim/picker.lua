--- Picker integration for selecting revisions/commits.
--- Uses vim.ui.select, which works with any picker (fzf-lua, telescope, dressing.nvim, etc.)
local M = {}

local function is_set(value)
    return value ~= nil and value ~= vim.NIL and value ~= ""
end

local function run_command(cmd)
    local lines = vim.fn.systemlist(cmd)
    if vim.v.shell_error ~= 0 then
        return nil
    end
    return lines
end

local function display_width(s)
    return vim.fn.strdisplaywidth(s)
end

local function pad_right(s, width)
    local pad = width - display_width(s)
    if pad <= 0 then
        return s
    end
    return s .. string.rep(" ", pad)
end

local function fit_description(desc)
    desc = desc ~= "" and desc or "(no description set)"
    if vim.fn.strchars(desc) > 40 then
        return vim.fn.strcharpart(desc, 0, 40) .. "..."
    end
    return pad_right(desc, 43)
end

local function git_items(limit, revspec, exclude_rev, include_staged)
    local cmd = {
        "git",
        "log",
        "--date=short",
        "--pretty=format:%H\t%h\t%ad\t%s",
        "-n",
        tostring(limit),
    }
    if is_set(revspec) then
        table.insert(cmd, revspec)
    end

    local lines = run_command(cmd)
    if not lines then
        return nil
    end

    local has_staged_changes = false
    if include_staged then
        vim.fn.system({ "git", "diff", "--cached", "--quiet" })
        has_staged_changes = (vim.v.shell_error == 1)
    end

    local items = {}
    if has_staged_changes then
        table.insert(items, {
            rev = "--staged",
            text = "(STAGED)",
        })
    end

    for _, line in ipairs(lines) do
        local full, short, date, subject = line:match("^([^\t]+)\t([^\t]+)\t([^\t]+)\t(.*)$")
        if full and short and full ~= exclude_rev then
            table.insert(items, {
                rev = full,
                short = short,
                date = date,
                subject = subject,
                text = string.format("%s  %s  %s", short, date or "", subject or ""),
            })
        end
    end
    return items
end

local function jj_items(limit, revset, exclude_rev)
    local cmd = { "jj", "log", "--no-graph", "-n", tostring(limit) }

    if is_set(revset) then
        table.insert(cmd, "-r")
        table.insert(cmd, revset)
    end

    table.insert(cmd, "-T")
    table.insert(
        cmd,
        'if(current_working_copy, "@", if(immutable, "◆", "○")) ++ "\\t" ++ description.first_line() ++ "\\t" ++ change_id.shortest() ++ "\\t" ++ author.timestamp().ago() ++ "\\t" ++ commit_id ++ "\\n"'
    )

    local lines = run_command(cmd)
    if not lines then
        return nil
    end

    local raw_items = {}
    local revset_w = 0
    for _, line in ipairs(lines) do
        local icon, desc, revset_id, age, rev = line:match("^([^\t]*)\t([^\t]*)\t([^\t]*)\t([^\t]*)\t([^\t]+)$")
        if rev and rev ~= exclude_rev then
            revset_w = math.max(revset_w, display_width(revset_id))
            table.insert(raw_items, {
                icon = icon,
                desc = desc,
                revset_id = revset_id,
                age = age,
                rev = rev,
            })
        end
    end

    local items = {}
    for _, item in ipairs(raw_items) do
        local desc = fit_description(item.desc)
        local revset = pad_right(item.revset_id, revset_w)
        local text = string.format(
            "%s %s %s %s",
            item.icon,
            desc,
            revset,
            item.age
        )
        table.insert(items, {
            rev = item.rev,
            revset_id = item.revset_id,
            desc = item.desc,
            text = text,
        })
    end

    return items
end

local function effective_jj_revset(opts, rev_filter)
    if is_set(rev_filter) and is_set(opts.jj_log_revset) then
        return string.format("(%s) & (%s)", rev_filter, opts.jj_log_revset)
    end
    if is_set(rev_filter) then
        return rev_filter
    end
    return opts.jj_log_revset
end

local function load_items(vcs, opts, rev_filter, exclude_rev, include_staged)
    if vcs == "git" then
        return git_items(opts.limit, rev_filter, exclude_rev, include_staged)
    end

    local jj_revset = effective_jj_revset(opts, rev_filter)
    return jj_items(opts.limit, jj_revset, exclude_rev)
end

local function open_picker(items, title, on_select)
    vim.ui.select(items, {
        prompt = title,
        format_item = function(item)
            return item.text
        end,
    }, function(choice)
        if choice and choice.rev then
            on_select(choice.rev)
        end
    end)
end

--- Open a picker and invoke callback with selected revision string.
--- @param vcs string
--- @param opts table
--- @param on_select fun(revset:string)
function M.pick(vcs, opts, on_select)
    local items = load_items(vcs, opts, nil, nil, true)

    if not items then
        vim.notify(string.format("Failed to load %s history", vcs), vim.log.levels.ERROR)
        return
    end
    if #items == 0 then
        vim.notify("No revisions found", vim.log.levels.INFO)
        return
    end

    local title = vcs == "git" and "Select git commit" or "Select jj revision"

    open_picker(items, title, on_select)
end

local function format_commit_label(short, date, subject)
    local clean = (subject or "")
        :gsub("[\r\n\t]", " ")
        :match("^%s*(.-)%s*$")
    if clean == "" then
        clean = "(no message)"
    elseif vim.fn.strchars(clean) > 30 then
        clean = vim.fn.strcharpart(clean, 0, 30) .. "..."
    end
    return string.format("%s %s %s", short or "", date or "", clean)
end

--- Open two pickers (new then old endpoint) and invoke callback.
--- @param vcs string
--- @param opts table
--- @param on_select fun(old_rev:string, new_rev:string)
function M.pick_compare(vcs, opts, on_select)
    local new_items = load_items(vcs, opts, nil, nil, false)
    if not new_items then
        vim.notify(string.format("Failed to load %s history", vcs), vim.log.levels.ERROR)
        return
    end
    if #new_items == 0 then
        vim.notify("No revisions found", vim.log.levels.INFO)
        return
    end

    -- Prepend special endpoints for git (working tree / staged)
    if vcs == "git" then
        -- Check for staged changes
        vim.fn.system({ "git", "diff", "--cached", "--quiet" })
        if vim.v.shell_error == 1 then
            table.insert(new_items, 1, {
                rev = "--staged",
                text = "STAGED (INDEX)",
            })
        end

        -- Check for unstaged changes (tracked or untracked)
        vim.fn.system({ "git", "diff", "--quiet" })
        local has_unstaged = (vim.v.shell_error == 1)
        if not has_unstaged then
            local untracked = vim.fn.systemlist({ "git", "ls-files", "--others", "--exclude-standard", ":/" })
            has_unstaged = (vim.v.shell_error == 0 and #untracked > 0)
        end
        if has_unstaged then
            table.insert(new_items, 1, {
                rev = "--working-tree",
                text = "WORKING TREE",
            })
        end
    end

    local new_title = "Compare: select new"
    open_picker(new_items, new_title, function(new_rev)
        -- Find the selected item to get display info
        local new_item
        for _, item in ipairs(new_items) do
            if item.rev == new_rev then
                new_item = item
                break
            end
        end

        local new_label
        if new_rev == "--working-tree" then
            new_label = "WORKING TREE"
        elseif new_rev == "--staged" then
            new_label = "STAGED"
        elseif new_rev == "--index" then
            new_label = "INDEX"
        elseif new_item and new_item.short then
            new_label = format_commit_label(new_item.short, new_item.date, new_item.subject)
        elseif new_item and new_item.revset_id then
            new_label = format_commit_label(new_item.revset_id, nil, new_item.desc)
        else
            new_label = new_rev:sub(1, 8)
        end

        local parent_filter
        local exclude_rev
        if new_rev == "--working-tree" or new_rev == "--staged" then
            parent_filter = nil
            exclude_rev = nil
        elseif vcs == "git" then
            parent_filter = new_rev
            exclude_rev = new_rev
        else
            parent_filter = string.format("(::%s) & (trunk()::)", new_rev)
            exclude_rev = new_rev
        end

        local old_items = load_items(vcs, opts, parent_filter, exclude_rev, false)
        if not old_items then
            vim.notify(string.format("Failed to load parent revisions for %s", new_label), vim.log.levels.ERROR)
            return
        end
        -- When comparing against working tree, INDEX is a valid old side (= git diff, working tree vs index)
        if new_rev == "--working-tree" and vcs == "git" then
            table.insert(old_items, 1, {
                rev = "--index",
                text = "INDEX",
            })
        end

        if #old_items == 0 then
            vim.notify("No parent revisions available for selected revision", vim.log.levels.WARN)
            return
        end
        local old_title = string.format("Select old (against %s)", new_label)
        open_picker(old_items, old_title, function(old_rev)
            on_select(old_rev, new_rev)
        end)
    end)
end

return M
