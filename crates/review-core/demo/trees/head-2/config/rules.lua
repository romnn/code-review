-- Categorization rules for imported bank statements: the first rule whose pattern matches a
-- line's memo chooses its account.
-- The importer loads this file with `require("rules")`.

local M = {}

-- Patterns are Lua patterns, matched against the upper-cased memo.
local rules = {
  { pattern = "^REWE", account = "expenses:food" },
  { pattern = "^SBB", account = "expenses:travel" },
  { pattern = "SALARY", account = "income:salary", sign = -1 },
  { pattern = "^ATM %d+", account = "assets:cash" },
}

--- Returns the account for a statement line, or nil when no rule matches.
---@param memo string
---@param amount_cents integer
---@return string|nil
function M.categorize(memo, amount_cents)
  local upper = memo:upper()
  for _, rule in ipairs(rules) do
    if upper:find(rule.pattern) then
      -- A rule with a sign only takes amounts with that sign.
      if rule.sign == nil or amount_cents * rule.sign > 0 then
        return rule.account
      end
    end
  end
  return nil
end

--- Counts the lines each account received, for the import summary.
---@param lines { memo: string, amount_cents: integer }[]
---@return string
function M.summary(lines)
  local counts = setmetatable({}, {
    __index = function()
      return 0
    end,
  })
  for _, line in ipairs(lines) do
    local account = M.categorize(line.memo, line.amount_cents) or "unmatched"
    counts[account] = counts[account] + 1
  end
  local rows = {}
  for account, count in pairs(counts) do
    rows[#rows + 1] = string.format("%-20s %5d", account, count)
  end
  table.sort(rows)
  return table.concat(rows, "\n")
end

return M
