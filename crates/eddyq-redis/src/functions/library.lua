#!lua name=eddyq_v1
-- ====================================================================
-- eddyq_v1 — Redis Functions library for the eddyq Redis backend.
--
-- All keys for a given line use the hash-tag `{<line>}` so they map to a
-- single Redis Cluster slot. KEYS[1] is the line prefix string (e.g.
-- "{main}") — every other key is built from it inside the function so all
-- writes happen against keys in the same slot.
--
-- Wire format (Rust → Lua):
--   KEYS[1] = "{<line>}"                e.g. "{main}"
--   ARGV    = positional arguments (function-specific)
--
-- Wire format (Lua → Rust):
--   Each function returns a flat array (Redis bulk-string array). The
--   shape is documented per function. Numeric return uses string
--   encoding so the Rust side parses uniformly.
-- ====================================================================

-- -------- helpers ----------------------------------------------------

local function jobkey(prefix, id)        return prefix .. ":job:"     .. id   end
local function errkey(prefix, id)        return prefix .. ":job:"     .. id .. ":errors" end
local function waitkey(prefix, q)        return prefix .. ":wait:"    .. q    end
local function delayedkey(prefix)        return prefix .. ":delayed"          end
local function activekey(prefix)         return prefix .. ":active"           end
local function completedkey(prefix)      return prefix .. ":completed"        end
local function failedkey(prefix)         return prefix .. ":failed"           end
local function cancelledkey(prefix)      return prefix .. ":cancelled"        end
local function queuesetkey(prefix, q)    return prefix .. ":queue:"   .. q    end
local function kindsetkey(prefix, k)     return prefix .. ":kind:"    .. k    end
local function tagsetkey(prefix, t)      return prefix .. ":tag:"     .. t    end
local function uniquekey(prefix, u)      return prefix .. ":unique:"  .. u    end
local function leaderkey(prefix, role)   return prefix .. ":leader:"  .. role end
local function wakeupchan(prefix)        return prefix .. ":wakeup"           end
local function resignchan(prefix)        return prefix .. ":resign"           end
local function groupmetakey(prefix, k)   return prefix .. ":group:"   .. k .. ":meta"    end
local function grouprunkey(prefix, k)    return prefix .. ":group:"   .. k .. ":running" end
local function groupsetkey(prefix)       return prefix .. ":groups"           end
local function queueseenkey(prefix)      return prefix .. ":queues:seen"      end
local function grouprulekey(prefix)      return prefix .. ":group_rules"      end
local function nqmetakey(prefix, q)      return prefix .. ":nq:"      .. q .. ":meta"    end
local function nqrunkey(prefix, q)       return prefix .. ":nq:"      .. q .. ":running" end
local function nqsetkey(prefix)          return prefix .. ":nqs"              end
-- Per-queue mirrors of the global state ZSETs. Maintained alongside the
-- global ZSETs (`:completed`, `:failed`, `:delayed`, `:cancelled`) so
-- dashboards can render correct per-queue counts. The global ZSETs remain
-- the source of truth for cross-queue `list_jobs` queries; the per-queue
-- mirrors exist for stats and per-queue listings.
local function nqcompletedkey(prefix, q) return prefix .. ":nq:"      .. q .. ":completed" end
local function nqfailedkey(prefix, q)    return prefix .. ":nq:"      .. q .. ":failed"    end
local function nqscheduledkey(prefix, q) return prefix .. ":nq:"      .. q .. ":scheduled" end
local function nqcancelledkey(prefix, q) return prefix .. ":nq:"      .. q .. ":cancelled" end
local function schedulekey(prefix)       return prefix .. ":schedules"        end
local function scheduleidxkey(prefix)    return prefix .. ":schedules:idx"    end
local function schedulenextkey(prefix)   return prefix .. ":schedules:next"   end
local function idgenkey(prefix)          return prefix .. ":idgen"            end

-- Wait-ZSET score: priority encoded into the high digits so jobs sort by
-- (priority desc, scheduled_at asc) in a single ZRANGE BYSCORE.
-- Negate priority so higher priority => lower score (sorts first).
local function wait_score(priority, scheduled_at_ms)
  return (-priority) * 1e13 + scheduled_at_ms
end

-- Append an entry to the per-job error list. We store as a JSON string per
-- entry so dashboards can render the structured fields.
local function push_error(prefix, id, error_json)
  redis.call('RPUSH', errkey(prefix, id), error_json)
end

-- Decode a JSON arg (Lua's cjson is loaded by Redis).
-- Empty string => nil (caller-friendly: "" means "not provided").
local function maybe_decode(s)
  if s == nil or s == '' then return nil end
  return cjson.decode(s)
end

-- Read the per-group meta into a Lua table with normalized defaults. Returns
-- nil if the group is unset (no admin call has registered it). Cheap — one
-- HMGET, no allocation in the common "no group" path.
local function read_group_meta(prefix, key)
  if key == nil or key == '' then return nil end
  local m = redis.call('HMGET', groupmetakey(prefix, key),
    'max_concurrency', 'paused',
    'rate_count', 'rate_period_ms',
    'tokens', 'tokens_refilled_at_ms')
  -- HMGET against a missing key returns an array of nils. If every field is
  -- false, treat the group as unconfigured (no limits).
  if m[1] == false and m[2] == false and m[3] == false then
    return nil
  end
  return {
    max_concurrency = tonumber(m[1]) or -1,
    paused          = (m[2] == '1' or m[2] == 1) and 1 or 0,
    rate_count      = tonumber(m[3]) or -1,
    rate_period_ms  = tonumber(m[4]) or 0,
    tokens          = tonumber(m[5]) or 0,
    refilled_at     = tonumber(m[6]) or 0,
  }
end

-- Token-bucket refill against the group meta in place. Persists the new
-- token count + refilled_at back to the HASH. Returns the *post-refill*
-- token count (caller decides whether to allow the claim and decrement).
local function refill_tokens(prefix, key, meta, now_ms)
  if meta.rate_count < 0 or meta.rate_period_ms <= 0 then
    return meta.tokens
  end
  local elapsed = now_ms - (meta.refilled_at or now_ms)
  if elapsed < 0 then elapsed = 0 end
  local refill = (elapsed * meta.rate_count) / meta.rate_period_ms
  local capped = math.min(meta.rate_count, meta.tokens + refill)
  meta.tokens = capped
  meta.refilled_at = now_ms
  return capped
end

local function persist_tokens(prefix, key, meta)
  redis.call('HSET', groupmetakey(prefix, key),
    'tokens',                tostring(meta.tokens),
    'tokens_refilled_at_ms', tostring(meta.refilled_at))
end

-- True if the group has any admin-configured constraint (concurrency, pause,
-- or rate). Used to decide whether claim has to gate.
local function group_has_limits(meta)
  return meta ~= nil
     and (meta.paused == 1
          or meta.max_concurrency >= 0
          or meta.rate_count >= 0)
end

-- Read the per-named-queue meta. Returns nil if the queue is unmanaged
-- (no admin call configured limits) — claim skips gating in that case.
local function read_nq_meta(prefix, name)
  if name == nil or name == '' then return nil end
  local m = redis.call('HMGET', nqmetakey(prefix, name),
    'max_concurrency', 'paused', 'default_timeout_ms')
  if m[1] == false and m[2] == false and m[3] == false then
    return nil
  end
  return {
    max_concurrency    = tonumber(m[1]) or -1,
    paused             = (m[2] == '1' or m[2] == 1) and 1 or 0,
    default_timeout_ms = tonumber(m[3]) or -1,
  }
end

local function nq_has_limits(meta)
  return meta ~= nil
     and (meta.paused == 1 or meta.max_concurrency >= 0)
end

-- Translate a glob ("customer-*", "tenant-?-prod") to a Lua pattern. `*`
-- becomes `.*`, `?` becomes `.`. All other Lua-magic characters are
-- escaped so they match literally.
local function glob_to_pattern(glob)
  local out = '^'
  local i = 1
  while i <= #glob do
    local c = string.sub(glob, i, i)
    if c == '*' then
      out = out .. '.*'
    elseif c == '?' then
      out = out .. '.'
    elseif string.find(c, '[%^%$%(%)%%%.%[%]%+%-]') then
      out = out .. '%' .. c
    else
      out = out .. c
    end
    i = i + 1
  end
  return out .. '$'
end

-- Best-matching rule for `group_key`. Highest priority wins; ties broken by
-- longest pattern. Returns nil if no rule matches. Each entry has shape
-- { max_concurrency?, rate_count?, rate_period_ms?, priority? }.
local function best_rule(prefix, group_key)
  local all = redis.call('HGETALL', grouprulekey(prefix))
  if #all == 0 then return nil end
  local best, best_prio, best_len = nil, -math.huge, -1
  for i = 1, #all, 2 do
    local pattern = all[i]
    local raw = all[i + 1]
    local ok, rule = pcall(cjson.decode, raw)
    if ok and rule then
      local pat = glob_to_pattern(pattern)
      if string.match(group_key, pat) then
        local p = tonumber(rule.priority) or 0
        local len = #pattern
        if p > best_prio or (p == best_prio and len > best_len) then
          best, best_prio, best_len = rule, p, len
        end
      end
    end
  end
  return best
end

-- Materialize a group meta HASH from a matching rule. No-op when the meta
-- already exists (admin already configured this group explicitly).
local function maybe_materialize_group(prefix, group_key, now_ms)
  if group_key == nil or group_key == '' then return end
  if redis.call('EXISTS', groupmetakey(prefix, group_key)) == 1 then return end
  local rule = best_rule(prefix, group_key)
  if rule == nil then return end
  -- JSON `null` decodes to `cjson.null` in Redis's cjson — distinct from
  -- nil. Treat both as "field not provided" so an unset rate doesn't
  -- silently install a `rate_count=0` (which would block every claim).
  local function present(v)
    return v ~= nil and v ~= cjson.null
  end
  local meta = {}
  if present(rule.max_concurrency) then
    meta['max_concurrency'] = tostring(tonumber(rule.max_concurrency) or 0)
  end
  if present(rule.rate_count) and present(rule.rate_period_ms) then
    meta['rate_count']            = tostring(tonumber(rule.rate_count) or 0)
    meta['rate_period_ms']        = tostring(tonumber(rule.rate_period_ms) or 0)
    meta['tokens']                = tostring(tonumber(rule.rate_count) or 0)
    meta['tokens_refilled_at_ms'] = tostring(now_ms)
  end
  -- Only HSET if we have any field — avoid creating an empty hash if the
  -- rule was malformed.
  local has = false
  for _ in pairs(meta) do has = true; break end
  if has then
    local args = { groupmetakey(prefix, group_key) }
    for k, v in pairs(meta) do
      args[#args + 1] = k
      args[#args + 1] = v
    end
    redis.call('HSET', unpack(args))
    redis.call('SADD', groupsetkey(prefix), group_key)
  end
end

-- ====================================================================
-- eddyq_enqueue (single)
--
-- ARGV:
--   1  kind                (string)
--   2  payload_json        (string)
--   3  priority            (int as string)
--   4  max_attempts        (int as string)
--   5  scheduled_at_ms     (int as string; 0 = "now")
--   6  unique_key          (string; "" = no dedup)
--   7  group_key           (string; "" = no group)
--   8  queue               (string)
--   9  tags_json           (string; "" or "[]" => no tags)
--  10  metadata_json       (string; "" => "{}")
--  11  remove_on_complete  (string JSON; "" => null)
--  12  remove_on_fail      (string JSON; "" => null)
--  13  now_ms              (int as string)
--
-- Returns: { "inserted", id }  or  { "skipped" }
-- ====================================================================
local function fn_enqueue(keys, args)
  local prefix              = keys[1]
  local kind                = args[1]
  local payload             = args[2]
  local priority            = tonumber(args[3])
  local max_attempts        = tonumber(args[4])
  local scheduled_at_ms     = tonumber(args[5])
  local unique_key          = args[6]
  local group_key           = args[7]
  local queue               = args[8]
  local tags_json           = args[9]
  local metadata_json       = args[10]
  local remove_on_complete  = args[11]
  local remove_on_fail      = args[12]
  local now_ms              = tonumber(args[13])

  -- Unique-key dedup: SET NX. If the key already maps to a job id, this
  -- enqueue is a no-op.
  if unique_key ~= '' then
    local got = redis.call('SET', uniquekey(prefix, unique_key), '1', 'NX')
    if not got then
      return { 'skipped' }
    end
  end

  local id = redis.call('INCR', prefix .. ':idgen')
  local effective_scheduled = scheduled_at_ms
  if effective_scheduled == 0 then effective_scheduled = now_ms end
  local due_now = effective_scheduled <= now_ms
  local state = due_now and 'pending' or 'scheduled'

  -- Job HASH — every field a primitive string so HMGET returns predictable
  -- types on the Rust side. JSON fields stored as their raw text.
  redis.call('HSET', jobkey(prefix, id),
    'id',                  id,
    'kind',                kind,
    'payload',             payload,
    'priority',            priority,
    'max_attempts',        max_attempts,
    'attempt',             0,
    'state',               state,
    'queue',               queue,
    'group_key',           group_key,
    'unique_key',          unique_key,
    'tags',                (tags_json == '' and '[]' or tags_json),
    'metadata',            (metadata_json == '' and '{}' or metadata_json),
    'remove_on_complete',  remove_on_complete,
    'remove_on_fail',      remove_on_fail,
    'scheduled_at',        effective_scheduled,
    'created_at',          now_ms
  )

  -- Filter sets — for list_jobs in PR3.
  redis.call('SADD', queuesetkey(prefix, queue), id)
  redis.call('SADD', kindsetkey(prefix, kind),   id)
  -- Track every queue we've seen so `get_stats` can enumerate without SCAN.
  redis.call('SADD', queueseenkey(prefix), queue)
  -- Pattern-based group rule materialization: if this is the first job we've
  -- seen for `group_key` and an admin has registered a matching rule, seed
  -- the group meta with the rule's defaults.
  if group_key ~= '' then
    maybe_materialize_group(prefix, group_key, now_ms)
  end
  if tags_json ~= '' and tags_json ~= '[]' then
    local tags = cjson.decode(tags_json)
    for _, t in ipairs(tags) do
      redis.call('SADD', tagsetkey(prefix, t), id)
    end
  end

  if due_now then
    redis.call('ZADD', waitkey(prefix, queue), wait_score(priority, effective_scheduled), id)
    -- Fire-and-forget wakeup. No subscriber == dropped, that's fine; the
    -- fetcher's poll-floor catches the next claim.
    redis.call('PUBLISH', wakeupchan(prefix), tostring(id))
  else
    redis.call('ZADD', delayedkey(prefix), effective_scheduled, id)
    redis.call('ZADD', nqscheduledkey(prefix, queue), effective_scheduled, id)
  end

  return { 'inserted', tostring(id) }
end

-- ====================================================================
-- eddyq_enqueue_many (batched)
--
-- ARGV layout:
--   1   n              (count of jobs)
--   for i in 1..n:
--     base = 1 + (i-1) * 12
--     args[base+1]  = kind
--     args[base+2]  = payload_json
--     args[base+3]  = priority
--     args[base+4]  = max_attempts
--     args[base+5]  = scheduled_at_ms
--     args[base+6]  = unique_key
--     args[base+7]  = group_key
--     args[base+8]  = queue
--     args[base+9]  = tags_json
--     args[base+10] = metadata_json
--     args[base+11] = remove_on_complete
--     args[base+12] = remove_on_fail
--   final: now_ms
--
-- Returns: a flat array length n*2 with pairs ("inserted", id) or ("skipped", "0")
-- ====================================================================
local function fn_enqueue_many(keys, args)
  local prefix = keys[1]
  local n = tonumber(args[1])
  local now_ms = tonumber(args[#args])
  local out = {}

  for i = 1, n do
    local base = 1 + (i - 1) * 12
    local sub_args = {
      args[base + 1],  args[base + 2],  args[base + 3],  args[base + 4],
      args[base + 5],  args[base + 6],  args[base + 7],  args[base + 8],
      args[base + 9],  args[base + 10], args[base + 11], args[base + 12],
      tostring(now_ms),
    }
    local r = fn_enqueue(keys, sub_args)
    out[#out + 1] = r[1]
    out[#out + 1] = r[2] or '0'
  end

  return out
end

-- ====================================================================
-- eddyq_claim (batch fetch)
--
-- ARGV:
--   1   batch_size      (int)
--   2   worker_id       (uuid string)
--   3   now_ms          (int)
--   4   stale_lease_ms  (int — heartbeat sweep threshold for idempotency)
--   5   nq              (number of queues)
--   6..5+nq             queues
--   6+nq                nk (number of kinds)
--   7+nq..6+nq+nk       kinds
--
-- Returns: flat array of JSON strings, one per claimed job.
-- ====================================================================
local function fn_claim(keys, args)
  local prefix = keys[1]
  local batch_size = tonumber(args[1])
  local worker_id  = args[2]
  local now_ms     = tonumber(args[3])
  -- args[4] reserved for stale_lease_ms (PR3 use)
  local nq = tonumber(args[5])
  local queues = {}
  for i = 1, nq do queues[i] = args[5 + i] end
  local nk_idx = 6 + nq
  local nk = tonumber(args[nk_idx])
  local kinds = {}
  local kinds_set = {}
  for i = 1, nk do
    kinds[i] = args[nk_idx + i]
    kinds_set[args[nk_idx + i]] = true
  end

  local out = {}
  local taken = 0

  for _, queue in ipairs(queues) do
    if taken >= batch_size then break end

    -- Take a generous slice up front so we have room to skip kind-mismatches
    -- without re-reading. ZRANGE returns lowest-score first = highest priority.
    local slice = redis.call('ZRANGE', waitkey(prefix, queue), 0, batch_size * 4 - 1)
    for _, id in ipairs(slice) do
      if taken >= batch_size then break end

      -- Read minimum needed fields. Skip if kind not in subscribed list.
      local fields = redis.call('HMGET', jobkey(prefix, id),
        'kind', 'payload', 'priority', 'max_attempts', 'attempt',
        'queue', 'group_key', 'state')
      local kind = fields[1]
      local gkey = fields[7]
      if kind and kinds_set[kind] and fields[8] == 'pending' then
        -- Gate on group: pause / concurrency cap / rate-limit token bucket.
        -- All three checks happen atomically inside this Lua call, so the
        -- ZCARD running can't race with another claimer in this slot.
        local allowed = true
        local meta = read_group_meta(prefix, gkey)
        if group_has_limits(meta) then
          if meta.paused == 1 then
            allowed = false
          end
          if allowed and meta.max_concurrency >= 0 then
            local running = redis.call('ZCARD', grouprunkey(prefix, gkey))
            if running >= meta.max_concurrency then
              allowed = false
            end
          end
          if allowed and meta.rate_count >= 0 then
            local tokens = refill_tokens(prefix, gkey, meta, now_ms)
            if tokens < 1.0 then
              persist_tokens(prefix, gkey, meta)
              allowed = false
            else
              meta.tokens = tokens - 1
              persist_tokens(prefix, gkey, meta)
            end
          end
        end

        -- Cross-process named-queue gating: a parallel mechanism to groups,
        -- bounded per-queue. Skipped when nothing's configured.
        if allowed then
          local nq_meta = read_nq_meta(prefix, queue)
          if nq_has_limits(nq_meta) then
            if nq_meta.paused == 1 then
              allowed = false
            end
            if allowed and nq_meta.max_concurrency >= 0 then
              local running = redis.call('ZCARD', nqrunkey(prefix, queue))
              if running >= nq_meta.max_concurrency then
                allowed = false
              end
            end
          end
        end

        if allowed then
          -- Atomically transition: ZREM from wait, ZADD to active, bump attempt,
          -- set lease metadata.
          local removed = redis.call('ZREM', waitkey(prefix, queue), id)
          if removed == 1 then
            local attempt = (tonumber(fields[5]) or 0) + 1
            redis.call('ZADD', activekey(prefix), now_ms, id)
            redis.call('HSET', jobkey(prefix, id),
              'state',     'running',
              'attempt',   attempt,
              'locked_at', now_ms,
              'locked_by', worker_id)
            if gkey and gkey ~= '' then
              redis.call('ZADD', grouprunkey(prefix, gkey), now_ms, id)
            end
            redis.call('ZADD', nqrunkey(prefix, queue), now_ms, id)

            -- Build the per-job JSON the Rust ClaimedJob constructor expects.
            out[#out + 1] = cjson.encode({
              id           = tonumber(id),
              kind         = kind,
              payload      = fields[2],          -- JSON string; Rust re-parses
              priority     = tonumber(fields[3]) or 0,
              max_attempts = tonumber(fields[4]) or 3,
              attempt      = attempt,
              queue        = fields[6],
              group_key    = (gkey ~= '' and gkey) or nil,
              worker_id    = worker_id,
            })
            taken = taken + 1
          end
        end
      end
    end
  end

  return out
end

-- ====================================================================
-- eddyq_heartbeat
--
-- ARGV: now_ms, then job ids
-- Returns: count of HSETs performed
-- ====================================================================
local function fn_heartbeat(keys, args)
  local prefix = keys[1]
  local now_ms = tonumber(args[1])
  local count = 0
  for i = 2, #args do
    local id = args[i]
    -- Only refresh if the job exists and is still running with a lease.
    local state = redis.call('HGET', jobkey(prefix, id), 'state')
    if state == 'running' then
      redis.call('HSET', jobkey(prefix, id), 'locked_at', now_ms)
      redis.call('ZADD', activekey(prefix), now_ms, id)
      count = count + 1
    end
  end
  return count
end

-- Helper: prune retention ZSET in place, honoring per-job rule JSON
-- (matches BullMQ's removeOnComplete / removeOnFail).
--
-- rule_json shapes accepted:
--   ""                     -- nil      => keep, no inline prune
--   "true"                 -- boolean  => drop (no ZADD)
--   "false"                -- boolean  => keep, no inline prune
--   "{\"count\":N}"        -- count    => keep last N (ZREMRANGEBYRANK)
--   "{\"age\":S}"          -- age secs => prune older than (now - age)
--   "{\"age\":S,\"count\":N}" -- both
--
-- Returns true if the job HASH should be deleted (rule == drop). `nqkey`
-- is the per-queue mirror ZSET (`nq:<q>:completed` etc.); when set, every
-- ZADD/ZREM/prune operation on `zsetkey` is mirrored to it so dashboards
-- can render per-queue counts. `nqkey` may be nil for code paths that
-- don't know the queue (shouldn't happen today — kept tolerant).
local function apply_retention(prefix, id, zsetkey, nqkey, rule_json, now_ms)
  local function add_both()
    redis.call('ZADD', zsetkey, now_ms, id)
    if nqkey then redis.call('ZADD', nqkey, now_ms, id) end
  end
  if rule_json == nil or rule_json == '' then
    add_both(); return false
  end
  local ok, rule = pcall(cjson.decode, rule_json)
  if not ok or rule == nil then
    add_both(); return false
  end
  -- Boolean shorthand
  if rule == true then
    return true
  end
  if rule == false then
    add_both(); return false
  end
  -- Object with age/count
  add_both()
  if rule.age and tonumber(rule.age) then
    local cutoff = now_ms - (tonumber(rule.age) * 1000)
    redis.call('ZREMRANGEBYSCORE', zsetkey, '-inf', '(' .. cutoff)
    if nqkey then redis.call('ZREMRANGEBYSCORE', nqkey, '-inf', '(' .. cutoff) end
  end
  if rule.count and tonumber(rule.count) then
    -- Keep newest N. Pruned independently per ZSET — global count keeps
    -- newest N across all queues; per-queue keeps newest N within the
    -- queue. Both are valid views, just scoped differently.
    local keep = tonumber(rule.count)
    local total = redis.call('ZCARD', zsetkey)
    if total > keep then
      redis.call('ZREMRANGEBYRANK', zsetkey, 0, total - keep - 1)
    end
    if nqkey then
      local ntotal = redis.call('ZCARD', nqkey)
      if ntotal > keep then
        redis.call('ZREMRANGEBYRANK', nqkey, 0, ntotal - keep - 1)
      end
    end
  end
  return false
end

-- Tear down a job's metadata after a `drop` retention. Removes the HASH,
-- error log, queue/kind/tag set memberships, and unique-key reservation.
local function delete_job(prefix, id)
  -- Read the index keys before we delete the HASH so we know what to clean.
  local fields = redis.call('HMGET', jobkey(prefix, id),
    'kind', 'queue', 'tags', 'unique_key')
  local kind   = fields[1]
  local queue  = fields[2]
  local tags_j = fields[3]
  local uniq   = fields[4]
  if queue and queue ~= '' then redis.call('SREM', queuesetkey(prefix, queue), id) end
  if kind  and kind  ~= '' then redis.call('SREM', kindsetkey (prefix, kind),  id) end
  if tags_j and tags_j ~= '' and tags_j ~= '[]' then
    local ok, tags = pcall(cjson.decode, tags_j)
    if ok and tags then
      for _, t in ipairs(tags) do
        redis.call('SREM', tagsetkey(prefix, t), id)
      end
    end
  end
  if uniq and uniq ~= '' then redis.call('DEL', uniquekey(prefix, uniq)) end
  redis.call('DEL', errkey(prefix, id))
  redis.call('DEL', jobkey(prefix, id))
end

-- ====================================================================
-- eddyq_complete
--
-- ARGV: id, worker_id, now_ms, result_json (or "")
-- Returns: 1 = completed, 0 = stale lease (worker no longer owns)
-- ====================================================================
local function fn_complete(keys, args)
  local prefix    = keys[1]
  local id        = args[1]
  local worker_id = args[2]
  local now_ms    = tonumber(args[3])
  local result    = args[4]

  local locked_by = redis.call('HGET', jobkey(prefix, id), 'locked_by')
  if locked_by ~= worker_id then
    -- Lease was stolen (e.g. sweep_stale moved the row to another worker).
    -- Treat as no-op so we don't double-finalize.
    return 0
  end

  redis.call('ZREM', activekey(prefix), id)
  local meta = redis.call('HMGET', jobkey(prefix, id), 'group_key', 'queue')
  local gkey = meta[1]
  local jqueue = meta[2]
  if gkey and gkey ~= '' then
    redis.call('ZREM', grouprunkey(prefix, gkey), id)
  end
  if jqueue and jqueue ~= '' then
    redis.call('ZREM', nqrunkey(prefix, jqueue), id)
  end
  redis.call('HSET', jobkey(prefix, id),
    'state',        'completed',
    'completed_at', now_ms,
    'result',       (result == '' and '' or result))

  local rule = redis.call('HGET', jobkey(prefix, id), 'remove_on_complete')
  local nqck = (jqueue and jqueue ~= '') and nqcompletedkey(prefix, jqueue) or nil
  local should_drop = apply_retention(prefix, id, completedkey(prefix), nqck, rule, now_ms)
  if should_drop then
    redis.call('ZREM', completedkey(prefix), id)
    if nqck then redis.call('ZREM', nqck, id) end
    delete_job(prefix, id)
  end
  return 1
end

-- ====================================================================
-- eddyq_fail
--
-- ARGV: id, worker_id, now_ms, error_json, retry_at_ms (or "-1" = no retry)
-- Returns: { state, attempt }
--   state ∈ "scheduled" (retry queued) | "failed" (DLQ) | "stale" (no-op)
-- ====================================================================
local function fn_fail(keys, args)
  local prefix      = keys[1]
  local id          = args[1]
  local worker_id   = args[2]
  local now_ms      = tonumber(args[3])
  local error_json  = args[4]
  local retry_at_ms = tonumber(args[5])

  local locked_by = redis.call('HGET', jobkey(prefix, id), 'locked_by')
  if locked_by ~= worker_id then
    return { 'stale', '0' }
  end

  push_error(prefix, id, error_json)
  redis.call('ZREM', activekey(prefix), id)
  local fields = redis.call('HMGET', jobkey(prefix, id),
    'attempt', 'priority', 'queue', 'remove_on_fail', 'group_key')
  local attempt = tonumber(fields[1]) or 0
  local priority = tonumber(fields[2]) or 0
  local queue = fields[3]
  local rule = fields[4]
  local gkey = fields[5]
  if gkey and gkey ~= '' then
    redis.call('ZREM', grouprunkey(prefix, gkey), id)
  end
  if queue and queue ~= '' then
    redis.call('ZREM', nqrunkey(prefix, queue), id)
  end

  if retry_at_ms ~= nil and retry_at_ms >= 0 then
    -- Schedule retry. If retry is "now" or in the past, push directly
    -- into wait; otherwise into delayed.
    if retry_at_ms <= now_ms then
      redis.call('HSET', jobkey(prefix, id),
        'state', 'pending', 'failed_at', now_ms)
      redis.call('ZADD', waitkey(prefix, queue), wait_score(priority, retry_at_ms), id)
      redis.call('PUBLISH', wakeupchan(prefix), tostring(id))
    else
      redis.call('HSET', jobkey(prefix, id),
        'state', 'scheduled', 'failed_at', now_ms,
        'scheduled_at', retry_at_ms)
      redis.call('ZADD', delayedkey(prefix), retry_at_ms, id)
      if queue and queue ~= '' then
        redis.call('ZADD', nqscheduledkey(prefix, queue), retry_at_ms, id)
      end
    end
    return { 'scheduled', tostring(attempt) }
  end

  -- Permanent failure / DLQ
  redis.call('HSET', jobkey(prefix, id),
    'state', 'failed', 'failed_at', now_ms)
  local nqfk = (queue and queue ~= '') and nqfailedkey(prefix, queue) or nil
  local should_drop = apply_retention(prefix, id, failedkey(prefix), nqfk, rule, now_ms)
  if should_drop then
    redis.call('ZREM', failedkey(prefix), id)
    if nqfk then redis.call('ZREM', nqfk, id) end
    delete_job(prefix, id)
  end
  return { 'failed', tostring(attempt) }
end

-- ====================================================================
-- eddyq_sweep_stale
--
-- ARGV: stale_before_ms, batch_max
-- Returns: count requeued (or DLQ'd if attempts exceeded)
-- ====================================================================
local function fn_sweep_stale(keys, args)
  local prefix          = keys[1]
  local stale_before_ms = tonumber(args[1])
  local batch_max       = tonumber(args[2])

  local stale = redis.call('ZRANGEBYSCORE', activekey(prefix),
    '-inf', stale_before_ms, 'LIMIT', 0, batch_max)
  local count = 0
  for _, id in ipairs(stale) do
    local fields = redis.call('HMGET', jobkey(prefix, id),
      'attempt', 'max_attempts', 'priority', 'queue', 'group_key')
    local attempt = tonumber(fields[1]) or 0
    local max_attempts = tonumber(fields[2]) or 3
    local priority = tonumber(fields[3]) or 0
    local queue = fields[4]
    local gkey = fields[5]

    redis.call('ZREM', activekey(prefix), id)
    if gkey and gkey ~= '' then
      redis.call('ZREM', grouprunkey(prefix, gkey), id)
    end
    if queue and queue ~= '' then
      redis.call('ZREM', nqrunkey(prefix, queue), id)
    end
    if attempt >= max_attempts then
      -- Out of retries. Push to failed ZSET (no per-job retention pruning
      -- here — sweep is a recovery path, not a normal terminate).
      redis.call('HSET', jobkey(prefix, id),
        'state', 'failed',
        'failed_at', stale_before_ms)
      redis.call('ZADD', failedkey(prefix), stale_before_ms, id)
      if queue and queue ~= '' then
        redis.call('ZADD', nqfailedkey(prefix, queue), stale_before_ms, id)
      end
      push_error(prefix, id, '{"name":"StaleSweep","message":"worker died, no retries left"}')
    else
      -- Retry: back into wait. Reset the lease so the next claimer wins.
      redis.call('HSET', jobkey(prefix, id),
        'state', 'pending', 'locked_by', '', 'locked_at', 0)
      redis.call('ZADD', waitkey(prefix, queue), wait_score(priority, stale_before_ms), id)
      redis.call('PUBLISH', wakeupchan(prefix), tostring(id))
    end
    count = count + 1
  end
  return count
end

-- ====================================================================
-- eddyq_promote_delayed
--
-- ARGV: now_ms, batch_max
-- Returns: count promoted
-- ====================================================================
local function fn_promote_delayed(keys, args)
  local prefix    = keys[1]
  local now_ms    = tonumber(args[1])
  local batch_max = tonumber(args[2])

  local due = redis.call('ZRANGEBYSCORE', delayedkey(prefix),
    '-inf', now_ms, 'LIMIT', 0, batch_max)
  local count = 0
  for _, id in ipairs(due) do
    local fields = redis.call('HMGET', jobkey(prefix, id),
      'priority', 'queue')
    local priority = tonumber(fields[1]) or 0
    local queue = fields[2]
    redis.call('ZREM', delayedkey(prefix), id)
    if queue and queue ~= '' then
      redis.call('ZREM', nqscheduledkey(prefix, queue), id)
    end
    redis.call('HSET', jobkey(prefix, id), 'state', 'pending')
    redis.call('ZADD', waitkey(prefix, queue), wait_score(priority, now_ms), id)
    count = count + 1
  end
  if count > 0 then
    redis.call('PUBLISH', wakeupchan(prefix), tostring(count))
  end
  return count
end

-- ====================================================================
-- eddyq_reclaim_in_flight
--
-- ARGV: now_ms, then job ids
-- Returns: count of jobs successfully moved active → wait
-- ====================================================================
local function fn_reclaim_in_flight(keys, args)
  local prefix = keys[1]
  local now_ms = tonumber(args[1])
  local count = 0
  for i = 2, #args do
    local id = args[i]
    local state = redis.call('HGET', jobkey(prefix, id), 'state')
    if state == 'running' then
      local fields = redis.call('HMGET', jobkey(prefix, id), 'priority', 'queue', 'group_key')
      local priority = tonumber(fields[1]) or 0
      local queue = fields[2]
      local gkey = fields[3]
      redis.call('ZREM', activekey(prefix), id)
      if gkey and gkey ~= '' then
        redis.call('ZREM', grouprunkey(prefix, gkey), id)
      end
      if queue and queue ~= '' then
        redis.call('ZREM', nqrunkey(prefix, queue), id)
      end
      redis.call('HSET', jobkey(prefix, id),
        'state', 'pending', 'locked_by', '', 'locked_at', 0)
      redis.call('ZADD', waitkey(prefix, queue), wait_score(priority, now_ms), id)
      count = count + 1
    end
  end
  if count > 0 then
    redis.call('PUBLISH', wakeupchan(prefix), tostring(count))
  end
  return count
end

-- ====================================================================
-- eddyq_cancel
--
-- ARGV: id, now_ms
-- Returns: 1 if cancelled (was pending/scheduled), 0 otherwise.
-- ====================================================================
local function fn_cancel(keys, args)
  local prefix = keys[1]
  local id     = args[1]
  local now_ms = tonumber(args[2])

  local fields = redis.call('HMGET', jobkey(prefix, id), 'state', 'queue')
  local state = fields[1]
  local queue = fields[2]
  if state == 'pending' then
    redis.call('ZREM', waitkey(prefix, queue), id)
  elseif state == 'scheduled' then
    redis.call('ZREM', delayedkey(prefix), id)
    if queue and queue ~= '' then
      redis.call('ZREM', nqscheduledkey(prefix, queue), id)
    end
  else
    -- Running / completed / failed / cancelled — soft-cancel of running
    -- happens in PR3 via cancel_requested HSET; for now we report 0.
    return 0
  end
  redis.call('HSET', jobkey(prefix, id),
    'state', 'cancelled', 'cancelled_at', now_ms)
  redis.call('ZADD', cancelledkey(prefix), now_ms, id)
  if queue and queue ~= '' then
    redis.call('ZADD', nqcancelledkey(prefix, queue), now_ms, id)
  end
  return 1
end

-- ====================================================================
-- eddyq_leader_try
--
-- ARGV: worker_id, lease_secs, now_ms, role
-- Returns: 1 = won/refreshed, 0 = lost
-- ====================================================================
local function fn_leader_try(keys, args)
  local prefix    = keys[1]
  local worker_id = args[1]
  local lease     = tonumber(args[2])
  local now_ms    = tonumber(args[3])
  local role      = args[4]

  local key = leaderkey(prefix, role)
  local current = redis.call('GET', key)
  if current == nil or current == false then
    redis.call('SET', key, worker_id, 'PX', lease * 1000)
    return 1
  end
  if current == worker_id then
    -- Refresh
    redis.call('PEXPIRE', key, lease * 1000)
    return 1
  end
  return 0
end

-- ====================================================================
-- eddyq_leader_resign
--
-- ARGV: worker_id, role
-- Returns: 1 if we resigned, 0 if we weren't holding the lease.
-- ====================================================================
local function fn_leader_resign(keys, args)
  local prefix    = keys[1]
  local worker_id = args[1]
  local role      = args[2]

  local key = leaderkey(prefix, role)
  local current = redis.call('GET', key)
  if current == worker_id then
    redis.call('DEL', key)
    redis.call('PUBLISH', resignchan(prefix), role)
    return 1
  end
  return 0
end

-- ====================================================================
-- eddyq_group_set_concurrency
--
-- ARGV: key, max
-- Sets max_concurrency for the group; registers it in the group index.
-- Returns: 1 always (admin op is idempotent).
-- ====================================================================
local function fn_group_set_concurrency(keys, args)
  local prefix = keys[1]
  local key    = args[1]
  local max    = tonumber(args[2])
  redis.call('HSET', groupmetakey(prefix, key), 'max_concurrency', max)
  redis.call('SADD', groupsetkey(prefix), key)
  return 1
end

-- ====================================================================
-- eddyq_group_set_paused
--
-- ARGV: key, paused (0|1)
-- Returns: 1
-- ====================================================================
local function fn_group_set_paused(keys, args)
  local prefix = keys[1]
  local key    = args[1]
  local p      = tonumber(args[2]) or 0
  redis.call('HSET', groupmetakey(prefix, key), 'paused', p)
  redis.call('SADD', groupsetkey(prefix), key)
  return 1
end

-- ====================================================================
-- eddyq_group_set_rate
--
-- ARGV: key, count, period_ms, now_ms
-- Initializes the token bucket to `count` tokens, refilled_at = now.
-- Returns: 1
-- ====================================================================
local function fn_group_set_rate(keys, args)
  local prefix    = keys[1]
  local key       = args[1]
  local count     = tonumber(args[2])
  local period_ms = tonumber(args[3])
  local now_ms    = tonumber(args[4])
  redis.call('HSET', groupmetakey(prefix, key),
    'rate_count',            count,
    'rate_period_ms',        period_ms,
    'tokens',                tostring(count),
    'tokens_refilled_at_ms', tostring(now_ms))
  redis.call('SADD', groupsetkey(prefix), key)
  return 1
end

-- ====================================================================
-- eddyq_group_clear_rate
--
-- ARGV: key
-- Removes the rate fields, leaving concurrency/pause untouched.
-- Returns: 1
-- ====================================================================
local function fn_group_clear_rate(keys, args)
  local prefix = keys[1]
  local key    = args[1]
  redis.call('HDEL', groupmetakey(prefix, key),
    'rate_count', 'rate_period_ms', 'tokens', 'tokens_refilled_at_ms')
  return 1
end

-- ====================================================================
-- eddyq_group_get
--
-- ARGV: key
-- Returns: flat array of fields, empty if the group has no meta.
--   ["key","<key>","running_count","<n>","max_concurrency","<n>",
--    "paused","0|1","rate_count","<n>","rate_period_ms","<n>",
--    "tokens","<float>","tokens_refilled_at_ms","<n>"]
-- ====================================================================
local function fn_group_get(keys, args)
  local prefix = keys[1]
  local key    = args[1]
  local meta = read_group_meta(prefix, key)
  if meta == nil then return {} end
  local running = redis.call('ZCARD', grouprunkey(prefix, key))
  return {
    'key',                   key,
    'running_count',         tostring(running),
    'max_concurrency',       tostring(meta.max_concurrency),
    'paused',                tostring(meta.paused),
    'rate_count',            tostring(meta.rate_count),
    'rate_period_ms',        tostring(meta.rate_period_ms),
    'tokens',                tostring(meta.tokens),
    'tokens_refilled_at_ms', tostring(meta.refilled_at),
  }
end

-- ====================================================================
-- eddyq_group_list
--
-- ARGV: (none)
-- Returns: flat array of group payloads (same shape as group_get per entry,
-- concatenated). Empty if no groups are registered.
-- ====================================================================
local function fn_group_list(keys, args)
  local prefix = keys[1]
  local members = redis.call('SMEMBERS', groupsetkey(prefix))
  local out = {}
  for _, k in ipairs(members) do
    local entry = fn_group_get(keys, { k })
    if #entry > 0 then
      out[#out + 1] = entry
    end
  end
  return out
end

-- ====================================================================
-- eddyq_group_set_rule
--
-- ARGV: pattern, rule_json (JSON: {max_concurrency?, rate_count?,
--       rate_period_ms?, priority?})
-- Returns: 1
-- ====================================================================
local function fn_group_set_rule(keys, args)
  local prefix  = keys[1]
  local pattern = args[1]
  local rule    = args[2]
  redis.call('HSET', grouprulekey(prefix), pattern, rule)
  return 1
end

-- ====================================================================
-- eddyq_group_remove_rule
--
-- ARGV: pattern
-- Returns: 1 if existed, 0 otherwise.
-- ====================================================================
local function fn_group_remove_rule(keys, args)
  local prefix  = keys[1]
  local pattern = args[1]
  return redis.call('HDEL', grouprulekey(prefix), pattern)
end

-- ====================================================================
-- eddyq_group_list_rules
--
-- Returns: flat array of pairs [pattern, rule_json, pattern, rule_json, ...]
-- ====================================================================
local function fn_group_list_rules(keys, args)
  local prefix = keys[1]
  return redis.call('HGETALL', grouprulekey(prefix))
end

-- ====================================================================
-- eddyq_queue_set_concurrency
--
-- ARGV: name, max
-- Returns: 1
-- ====================================================================
local function fn_queue_set_concurrency(keys, args)
  local prefix = keys[1]
  local name   = args[1]
  local max    = tonumber(args[2])
  redis.call('HSET', nqmetakey(prefix, name), 'max_concurrency', max)
  redis.call('SADD', nqsetkey(prefix), name)
  return 1
end

-- ====================================================================
-- eddyq_queue_set_paused
--
-- ARGV: name, paused (0|1)
-- Returns: 1
-- ====================================================================
local function fn_queue_set_paused(keys, args)
  local prefix = keys[1]
  local name   = args[1]
  local p      = tonumber(args[2]) or 0
  redis.call('HSET', nqmetakey(prefix, name), 'paused', p)
  redis.call('SADD', nqsetkey(prefix), name)
  return 1
end

-- ====================================================================
-- eddyq_queue_set_timeout
--
-- ARGV: name, timeout_ms (-1 = clear)
-- Returns: 1
-- ====================================================================
local function fn_queue_set_timeout(keys, args)
  local prefix    = keys[1]
  local name      = args[1]
  local timeout   = tonumber(args[2])
  if timeout == nil or timeout < 0 then
    redis.call('HDEL', nqmetakey(prefix, name), 'default_timeout_ms')
  else
    redis.call('HSET', nqmetakey(prefix, name), 'default_timeout_ms', timeout)
    redis.call('SADD', nqsetkey(prefix), name)
  end
  return 1
end

-- ====================================================================
-- eddyq_queue_get
--
-- ARGV: name
-- Returns: flat array or empty if unmanaged.
-- ====================================================================
local function fn_queue_get(keys, args)
  local prefix = keys[1]
  local name   = args[1]
  local meta = read_nq_meta(prefix, name)
  if meta == nil then return {} end
  local running = redis.call('ZCARD', nqrunkey(prefix, name))
  return {
    'name',               name,
    'running_count',      tostring(running),
    'max_concurrency',    tostring(meta.max_concurrency),
    'paused',             tostring(meta.paused),
    'default_timeout_ms', tostring(meta.default_timeout_ms),
  }
end

-- ====================================================================
-- eddyq_queue_list
-- ====================================================================
local function fn_queue_list(keys, args)
  local prefix = keys[1]
  local members = redis.call('SMEMBERS', nqsetkey(prefix))
  local out = {}
  for _, n in ipairs(members) do
    local entry = fn_queue_get(keys, { n })
    if #entry > 0 then out[#out + 1] = entry end
  end
  return out
end

-- ====================================================================
-- eddyq_schedule_upsert
--
-- ARGV: name, cron, kind, payload_json, priority, max_attempts, queue,
--       enabled (0|1), next_run_at_ms, [interval_ms]
--
-- `interval_ms` is optional (default 0). When > 0 the schedule fires on
-- a fixed interval — `next_run_at_ms` is computed at fire time as
-- `now_ms + interval_ms` and the `cron` field is ignored. When 0 the
-- schedule is cron-driven and the caller pre-computes `next_run_at_ms`
-- by expanding `cron`.
--
-- Returns: 1
-- ====================================================================
local function fn_schedule_upsert(keys, args)
  local prefix = keys[1]
  local name   = args[1]
  local cron   = args[2]
  local kind   = args[3]
  local payload = args[4]
  local priority = tonumber(args[5]) or 0
  local max_attempts = tonumber(args[6]) or 3
  local queue  = args[7]
  local enabled = tonumber(args[8]) or 1
  local next_run = tonumber(args[9]) or 0
  local interval_ms = tonumber(args[10]) or 0

  -- Preserve last_run_at across upserts (sync_schedules shouldn't clobber it).
  local prior = redis.call('HGET', schedulekey(prefix), name)
  local last_run = 0
  if prior then
    local ok, p = pcall(cjson.decode, prior)
    if ok and p and p.last_run_at_ms then last_run = p.last_run_at_ms end
  end

  local entry = cjson.encode({
    cron           = cron,
    kind           = kind,
    payload        = payload,
    priority       = priority,
    max_attempts   = max_attempts,
    queue          = queue,
    enabled        = enabled,
    last_run_at_ms = last_run,
    next_run_at_ms = next_run,
    interval_ms    = interval_ms,
  })
  redis.call('HSET', schedulekey(prefix), name, entry)
  redis.call('SADD', scheduleidxkey(prefix), name)
  if enabled == 1 then
    redis.call('ZADD', schedulenextkey(prefix), next_run, name)
  else
    redis.call('ZREM', schedulenextkey(prefix), name)
  end
  return 1
end

-- ====================================================================
-- eddyq_schedule_remove
--
-- ARGV: name
-- Returns: 1 if existed, 0 otherwise.
-- ====================================================================
local function fn_schedule_remove(keys, args)
  local prefix = keys[1]
  local name   = args[1]
  local removed = redis.call('HDEL', schedulekey(prefix), name)
  redis.call('SREM', scheduleidxkey(prefix), name)
  redis.call('ZREM', schedulenextkey(prefix), name)
  return removed
end

-- ====================================================================
-- eddyq_schedule_set_enabled
--
-- ARGV: name, enabled (0|1), next_run_at_ms_if_enabled
-- Returns: 1 if existed, 0 otherwise.
-- ====================================================================
local function fn_schedule_set_enabled(keys, args)
  local prefix = keys[1]
  local name   = args[1]
  local enabled = tonumber(args[2]) or 0
  local next_run = tonumber(args[3]) or 0

  local raw = redis.call('HGET', schedulekey(prefix), name)
  if not raw then return 0 end
  local entry = cjson.decode(raw)
  entry.enabled = enabled
  if enabled == 1 then
    -- Caller computed next_run via cron crate; honor it.
    entry.next_run_at_ms = next_run
    redis.call('ZADD', schedulenextkey(prefix), next_run, name)
  else
    redis.call('ZREM', schedulenextkey(prefix), name)
  end
  redis.call('HSET', schedulekey(prefix), name, cjson.encode(entry))
  return 1
end

-- ====================================================================
-- eddyq_schedule_list
--
-- ARGV: (none)
-- Returns: flat array of pairs [name, entry_json, name, entry_json, ...]
-- ====================================================================
local function fn_schedule_list(keys, args)
  local prefix = keys[1]
  return redis.call('HGETALL', schedulekey(prefix))
end

-- ====================================================================
-- eddyq_schedule_due_list
--
-- ARGV: now_ms, batch_max
-- Returns: flat array of pairs [name, entry_json, name, entry_json, ...]
-- ====================================================================
local function fn_schedule_due_list(keys, args)
  local prefix    = keys[1]
  local now_ms    = tonumber(args[1])
  local batch_max = tonumber(args[2])
  local names = redis.call('ZRANGEBYSCORE', schedulenextkey(prefix),
    '-inf', now_ms, 'LIMIT', 0, batch_max)
  local out = {}
  for _, n in ipairs(names) do
    local raw = redis.call('HGET', schedulekey(prefix), n)
    if raw then
      out[#out + 1] = n
      out[#out + 1] = raw
    end
  end
  return out
end

-- ====================================================================
-- eddyq_schedule_fire
--
-- Atomically: (a) enqueue a fresh job from the schedule, (b) bump
-- last_run_at = now, next_run_at = caller-computed cron next.
--
-- ARGV: name, kind, payload_json, priority, max_attempts, queue, now_ms,
--       next_run_at_ms
-- Returns: enqueued job id (as string) or "0" if the schedule vanished.
-- ====================================================================
local function fn_schedule_fire(keys, args)
  local prefix = keys[1]
  local name   = args[1]
  local kind   = args[2]
  local payload = args[3]
  local priority = tonumber(args[4]) or 0
  local max_attempts = tonumber(args[5]) or 3
  local queue  = args[6]
  local now_ms = tonumber(args[7])
  local next_run = tonumber(args[8]) or 0

  -- Confirm the schedule still exists. Removal between due_list and fire
  -- means we just no-op.
  local raw = redis.call('HGET', schedulekey(prefix), name)
  if not raw then return '0' end

  local id = redis.call('INCR', idgenkey(prefix))
  redis.call('HSET', jobkey(prefix, id),
    'id',            id,
    'kind',          kind,
    'payload',       payload,
    'priority',      priority,
    'max_attempts',  max_attempts,
    'attempt',       0,
    'state',         'pending',
    'queue',         queue,
    'group_key',     '',
    'unique_key',    '',
    'tags',          '[]',
    'metadata',      '{}',
    'remove_on_complete', '',
    'remove_on_fail',     '',
    'scheduled_at',  now_ms,
    'created_at',    now_ms
  )
  redis.call('SADD', queuesetkey(prefix, queue), id)
  redis.call('SADD', kindsetkey(prefix, kind),   id)
  redis.call('SADD', queueseenkey(prefix), queue)
  redis.call('ZADD', waitkey(prefix, queue), wait_score(priority, now_ms), id)
  redis.call('PUBLISH', wakeupchan(prefix), tostring(id))

  -- Advance the schedule. Skip-missed semantics: one enqueue per tick,
  -- next_run jumps to caller's cron-computed value even if it skips runs.
  local entry = cjson.decode(raw)
  entry.last_run_at_ms = now_ms
  entry.next_run_at_ms = next_run
  redis.call('HSET', schedulekey(prefix), name, cjson.encode(entry))
  redis.call('ZADD', schedulenextkey(prefix), next_run, name)

  return tostring(id)
end

-- ====================================================================
-- eddyq_schedule_sync_diff
--
-- Diff helper: returns the set of stored schedule names not in the
-- caller-provided keep-list. Rust then calls schedule_remove on each.
--
-- ARGV: n, then n names to KEEP.
-- Returns: array of names to delete.
-- ====================================================================
local function fn_schedule_sync_diff(keys, args)
  local prefix = keys[1]
  local n = tonumber(args[1]) or 0
  local keep = {}
  for i = 1, n do keep[args[1 + i]] = true end
  local all = redis.call('SMEMBERS', scheduleidxkey(prefix))
  local to_delete = {}
  for _, name in ipairs(all) do
    if not keep[name] then to_delete[#to_delete + 1] = name end
  end
  return to_delete
end

-- ====================================================================
-- eddyq_get_stats
--
-- Snapshot of job counts grouped by (queue, state). Walks the
-- `queues:seen` set to enumerate every queue eddyq has touched on this
-- line. Per-queue `pending` and `running` come from existing ZSETs;
-- `scheduled`/`completed`/`failed`/`cancelled` come from the global ZSETs
-- (those aren't partitioned per queue today — surfaced as a synthetic
-- "_global" queue so the dashboard can still show totals).
--
-- ARGV: (none)
-- Returns: flat array of (queue, state, count, ...) triples encoded as
--   single strings: "queue|state|count" per entry. Keeps the protocol
--   shape predictable.
-- ====================================================================
local function fn_get_stats(keys, args)
  local prefix = keys[1]
  local out = {}
  local queues = redis.call('SMEMBERS', queueseenkey(prefix))
  local accounted = {}  -- ids attributed to a per-queue mirror (drives _global remainder)
  for _, q in ipairs(queues) do
    local pending = redis.call('ZCARD', waitkey(prefix, q))
    if pending > 0 then
      out[#out + 1] = q .. '|pending|' .. tostring(pending)
    end
    local running = redis.call('ZCARD', nqrunkey(prefix, q))
    if running > 0 then
      out[#out + 1] = q .. '|running|' .. tostring(running)
    end
    local mirrors = {
      { 'scheduled', nqscheduledkey(prefix, q) },
      { 'completed', nqcompletedkey(prefix, q) },
      { 'failed',    nqfailedkey   (prefix, q) },
      { 'cancelled', nqcancelledkey(prefix, q) },
    }
    for _, m in ipairs(mirrors) do
      local n = redis.call('ZCARD', m[2])
      if n > 0 then
        out[#out + 1] = q .. '|' .. m[1] .. '|' .. tostring(n)
        if not accounted[m[1]] then accounted[m[1]] = 0 end
        accounted[m[1]] = accounted[m[1]] + n
      end
    end
  end
  -- Remainder: any global-ZSET entries not yet covered by the per-queue
  -- mirrors get surfaced under `_global` so historical jobs (predating the
  -- per-queue mirrors) still appear in totals. Once a backfill / new
  -- terminal transitions catch up, this row goes to zero and disappears.
  local globals = {
    { 'scheduled', delayedkey(prefix) },
    { 'completed', completedkey(prefix) },
    { 'failed',    failedkey(prefix) },
    { 'cancelled', cancelledkey(prefix) },
  }
  for _, g in ipairs(globals) do
    local total = redis.call('ZCARD', g[2])
    local mirrored = accounted[g[1]] or 0
    local remainder = total - mirrored
    if remainder > 0 then
      out[#out + 1] = '_global|' .. g[1] .. '|' .. tostring(remainder)
    end
  end
  return out
end

-- ====================================================================
-- eddyq_list_jobs
--
-- Paginated id-listing for the dashboard. Returns up to `limit` job ids
-- newest-first from the source set implied by `state`, plus the total
-- count for that state.
--
-- ARGV:
--   1  state         "pending"|"running"|"scheduled"|"completed"|
--                    "failed"|"cancelled"|"any"
--   2  queue         filter ("" = no constraint)
--   3  offset        (int)
--   4  limit         (int, capped at 500 client-side)
-- Returns: [ total, id, id, id, ... ]
-- ====================================================================
local function fn_list_jobs(keys, args)
  local prefix = keys[1]
  local state  = args[1] or 'any'
  local queue  = args[2] or ''
  local offset = tonumber(args[3]) or 0
  local limit  = tonumber(args[4]) or 50
  if limit < 1 then limit = 1 end
  if limit > 500 then limit = 500 end

  local function range_zset(key)
    local total = redis.call('ZCARD', key)
    local stop = offset + limit - 1
    local ids = redis.call('ZREVRANGE', key, offset, stop)
    return total, ids
  end

  -- `pending` is per-queue (wait:<q>), everything else is global.
  if state == 'pending' then
    if queue ~= '' then
      local total, ids = range_zset(waitkey(prefix, queue))
      local out = { tostring(total) }
      for _, id in ipairs(ids) do out[#out + 1] = id end
      return out
    end
    -- Pending across all queues: union of wait:<q> ZCARDs + paginate by
    -- walking each. Cheap because dashboards rarely set offset huge.
    local queues = redis.call('SMEMBERS', queueseenkey(prefix))
    local all_ids = {}
    local total = 0
    for _, q in ipairs(queues) do
      total = total + redis.call('ZCARD', waitkey(prefix, q))
      local ids = redis.call('ZREVRANGE', waitkey(prefix, q), 0, -1)
      for _, id in ipairs(ids) do all_ids[#all_ids + 1] = id end
    end
    local out = { tostring(total) }
    for i = offset + 1, math.min(offset + limit, #all_ids) do
      out[#out + 1] = all_ids[i]
    end
    return out
  end

  local source = nil
  if state == 'running' then    source = activekey(prefix)
  elseif state == 'scheduled' then source = delayedkey(prefix)
  elseif state == 'completed' then source = completedkey(prefix)
  elseif state == 'failed' then  source = failedkey(prefix)
  elseif state == 'cancelled' then source = cancelledkey(prefix)
  end

  if source then
    local total, ids = range_zset(source)
    local out = { tostring(total) }
    -- Queue filter: intersect by HMGET'ing the queue field on the page. Cheap
    -- since `limit` caps at 500.
    if queue ~= '' then
      local filtered = {}
      local kept = 0
      for _, id in ipairs(ids) do
        local jq = redis.call('HGET', jobkey(prefix, id), 'queue')
        if jq == queue then
          filtered[#filtered + 1] = id
          kept = kept + 1
        end
      end
      for _, id in ipairs(filtered) do out[#out + 1] = id end
    else
      for _, id in ipairs(ids) do out[#out + 1] = id end
    end
    return out
  end

  -- state == "any": concatenate from each pool, newest-first across states.
  local pools = {
    activekey(prefix), waitkey(prefix, ''), delayedkey(prefix),
    completedkey(prefix), failedkey(prefix), cancelledkey(prefix),
  }
  local total = 0
  local merged = {}
  for _, key in ipairs(pools) do
    if key ~= waitkey(prefix, '') then
      total = total + redis.call('ZCARD', key)
      local ids = redis.call('ZREVRANGE', key, 0, math.max(0, offset + limit - 1))
      for _, id in ipairs(ids) do merged[#merged + 1] = id end
    end
  end
  -- Pending from per-queue wait sets too.
  local queues = redis.call('SMEMBERS', queueseenkey(prefix))
  for _, q in ipairs(queues) do
    total = total + redis.call('ZCARD', waitkey(prefix, q))
    local ids = redis.call('ZREVRANGE', waitkey(prefix, q), 0, math.max(0, offset + limit - 1))
    for _, id in ipairs(ids) do merged[#merged + 1] = id end
  end
  local out = { tostring(total) }
  local hi = math.min(offset + limit, #merged)
  for i = offset + 1, hi do out[#out + 1] = merged[i] end
  return out
end

-- ====================================================================
-- eddyq_cleanup
--
-- Queue-default retention sweep for the three finalized ZSETs (completed,
-- failed, cancelled). Per-job retention runs inline in `apply_retention`
-- on complete/fail; this is the fallback for jobs whose owner didn't set
-- a per-job rule, plus the `false` opt-out path.
--
-- For each state, a row is reaped if it exceeds *either* the age window
-- *or* the count cap (OR semantics, BullMQ-style). Up to `batch_limit`
-- victims per state per call so a backlog can't block the event loop.
--
-- ARGV: now_ms,
--       completed_age_secs, failed_age_secs, cancelled_age_secs,
--       completed_count,    failed_count,    cancelled_count,
--       batch_limit
--   age < 0   => skip age check for that state.
--   age >= 0  => sweep entries finalized more than `age` seconds ago.
--                `age = 0` means "sweep everything older than now" — used
--                by `clean(grace=0, ...)` for an immediate ad-hoc prune.
--   count < 0 => no count cap for that state.
--   count >= 0=> keep at most `count` newest entries (by `finalized_at`).
--   batch_limit caps total work per state per call. Default 500 if 0.
-- Returns: { n_completed_deleted, n_failed_deleted, n_cancelled_deleted, 0 }
--   The 4th slot is reserved for Redis batch retention (not yet implemented;
--   batches don't have a dedicated finalized ZSET on Redis today).
-- ====================================================================
local function fn_cleanup(keys, args)
  local prefix      = keys[1]
  local now_ms      = tonumber(args[1])
  -- Default to -1 (skip) when missing — never 0, which means "sweep now"
  -- (age) or "delete everything" (count).
  local age_c       = tonumber(args[2]) or -1
  local age_f       = tonumber(args[3]) or -1
  local age_x       = tonumber(args[4]) or -1
  local cnt_c       = tonumber(args[5]) or -1
  local cnt_f       = tonumber(args[6]) or -1
  local cnt_x       = tonumber(args[7]) or -1
  local batch_limit = tonumber(args[8]) or 0
  if batch_limit <= 0 then batch_limit = 500 end

  -- Per-state sweep: collect the union of (age victims, count victims)
  -- capped at `batch_limit`, tear each down via `delete_job` (HASH + indexes
  -- + unique-key + error log), then ZREM from the finalized ZSET. Order:
  -- completed, failed, cancelled — matching the Retention struct slot order
  -- in Rust.
  local function sweep(zsetkey, nq_key_fn, age_secs, count)
    if age_secs < 0 and count < 0 then return 0 end
    local victims = {}
    local seen = {}
    -- Age-based: oldest scores first.
    if age_secs >= 0 then
      local cutoff = now_ms - (age_secs * 1000)
      local ids = redis.call('ZRANGEBYSCORE', zsetkey,
                             '-inf', '(' .. cutoff,
                             'LIMIT', 0, batch_limit)
      for _, id in ipairs(ids) do
        if not seen[id] then
          seen[id] = true
          victims[#victims + 1] = id
          if #victims >= batch_limit then break end
        end
      end
    end
    -- Count-cap: ZRANGE 0 to -(count+1) selects everything except the
    -- newest `count`. Out-of-range stop → empty result (Redis-safe).
    if count >= 0 and #victims < batch_limit then
      local stop = -(count + 1)
      local ids = redis.call('ZRANGE', zsetkey, 0, stop)
      for _, id in ipairs(ids) do
        if not seen[id] then
          seen[id] = true
          victims[#victims + 1] = id
          if #victims >= batch_limit then break end
        end
      end
    end
    for _, id in ipairs(victims) do
      -- Read queue before `delete_job` tears the HASH down — we need it
      -- to drop the matching per-queue mirror ZSET entry.
      local queue = redis.call('HGET', jobkey(prefix, id), 'queue')
      delete_job(prefix, id)
      redis.call('ZREM', zsetkey, id)
      if queue and queue ~= '' then
        redis.call('ZREM', nq_key_fn(prefix, queue), id)
      end
    end
    return #victims
  end

  local n_c = sweep(completedkey(prefix), nqcompletedkey, age_c, cnt_c)
  local n_f = sweep(failedkey(prefix),    nqfailedkey,    age_f, cnt_f)
  local n_x = sweep(cancelledkey(prefix), nqcancelledkey, age_x, cnt_x)

  return { tostring(n_c), tostring(n_f), tostring(n_x), '0' }
end

-- ====================================================================
-- eddyq_backfill_nq_states
--
-- One-shot migration: walk each global terminal-state ZSET and add any
-- entries missing from the per-queue mirror ZSETs (`nq:<q>:completed`,
-- etc.). Idempotent via `ZADD NX` — re-running re-scans but doesn't
-- duplicate. Use after upgrading to per-queue mirrors so historical jobs
-- get attributed to the right queue and stop showing as `_global`.
--
-- ARGV: batch_limit (optional, defaults to 5000) per source ZSET. The
-- function returns the number of mirrors added; caller can invoke again
-- if work remains (returned count == batch_limit on any source ⇒ more
-- to do). For typical admin-job backlogs (<10k terminal entries) a
-- single call suffices.
--
-- Returns: total entries newly inserted into per-queue mirrors.
-- ====================================================================
local function fn_backfill_nq_states(keys, args)
  local prefix      = keys[1]
  local batch_limit = tonumber(args and args[1]) or 5000
  if batch_limit <= 0 then batch_limit = 5000 end

  local sources = {
    { delayedkey(prefix),   nqscheduledkey },
    { completedkey(prefix), nqcompletedkey },
    { failedkey(prefix),    nqfailedkey    },
    { cancelledkey(prefix), nqcancelledkey },
  }
  local inserted = 0
  for _, src in ipairs(sources) do
    local entries = redis.call('ZRANGE', src[1], 0, batch_limit - 1, 'WITHSCORES')
    for i = 1, #entries, 2 do
      local id = entries[i]
      local score = entries[i + 1]
      local queue = redis.call('HGET', jobkey(prefix, id), 'queue')
      if queue and queue ~= '' then
        local added = redis.call('ZADD', src[2](prefix, queue), 'NX', score, id)
        if added == 1 then
          inserted = inserted + 1
          -- Mark the queue as seen so `fn_get_stats` enumerates it.
          -- Without this, a queue whose only state is `completed`/`failed`
          -- (no live pending or running jobs) would never appear in the
          -- dashboard, leaving its mirrored counts invisible.
          redis.call('SADD', queueseenkey(prefix), queue)
        end
      end
    end
  end
  return inserted
end

-- ====================================================================
-- registration
-- ====================================================================
redis.register_function('eddyq_enqueue',          fn_enqueue)
redis.register_function('eddyq_enqueue_many',     fn_enqueue_many)
redis.register_function('eddyq_claim',            fn_claim)
redis.register_function('eddyq_heartbeat',        fn_heartbeat)
redis.register_function('eddyq_complete',         fn_complete)
redis.register_function('eddyq_fail',             fn_fail)
redis.register_function('eddyq_sweep_stale',      fn_sweep_stale)
redis.register_function('eddyq_promote_delayed',  fn_promote_delayed)
redis.register_function('eddyq_reclaim_in_flight', fn_reclaim_in_flight)
redis.register_function('eddyq_cancel',           fn_cancel)
redis.register_function('eddyq_leader_try',       fn_leader_try)
redis.register_function('eddyq_leader_resign',    fn_leader_resign)
redis.register_function('eddyq_group_set_concurrency', fn_group_set_concurrency)
redis.register_function('eddyq_group_set_paused',      fn_group_set_paused)
redis.register_function('eddyq_group_set_rate',        fn_group_set_rate)
redis.register_function('eddyq_group_clear_rate',      fn_group_clear_rate)
redis.register_function('eddyq_group_get',             fn_group_get)
redis.register_function('eddyq_group_list',            fn_group_list)
redis.register_function('eddyq_schedule_upsert',       fn_schedule_upsert)
redis.register_function('eddyq_schedule_remove',       fn_schedule_remove)
redis.register_function('eddyq_schedule_set_enabled',  fn_schedule_set_enabled)
redis.register_function('eddyq_schedule_list',         fn_schedule_list)
redis.register_function('eddyq_schedule_due_list',     fn_schedule_due_list)
redis.register_function('eddyq_schedule_fire',         fn_schedule_fire)
redis.register_function('eddyq_schedule_sync_diff',    fn_schedule_sync_diff)
redis.register_function('eddyq_queue_set_concurrency', fn_queue_set_concurrency)
redis.register_function('eddyq_queue_set_paused',      fn_queue_set_paused)
redis.register_function('eddyq_queue_set_timeout',     fn_queue_set_timeout)
redis.register_function('eddyq_queue_get',             fn_queue_get)
redis.register_function('eddyq_queue_list',            fn_queue_list)
redis.register_function('eddyq_get_stats',             fn_get_stats)
redis.register_function('eddyq_list_jobs',             fn_list_jobs)
redis.register_function('eddyq_group_set_rule',        fn_group_set_rule)
redis.register_function('eddyq_group_remove_rule',     fn_group_remove_rule)
redis.register_function('eddyq_group_list_rules',      fn_group_list_rules)
redis.register_function('eddyq_cleanup',               fn_cleanup)
redis.register_function('eddyq_backfill_nq_states',    fn_backfill_nq_states)
