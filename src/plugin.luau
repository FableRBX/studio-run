--!nocheck
-- Bootstrap plugin written by studio-run for a single run and deleted when the
-- run ends. It checks in with the studio-run process that launched this
-- Studio, runs the bundled script, and streams Studio's output back until the
-- script finishes.

local RunService = game:GetService("RunService")

-- Local plugins also load into play-test DataModels. Only the edit DataModel
-- should ever run the script.
if not RunService:IsEdit() then
	return
end

local HttpService = game:GetService("HttpService")
local LogService = game:GetService("LogService")

local BASE_URL = "http://127.0.0.1:{{PORT}}"
local TOKEN = "{{TOKEN}}"
local FLUSH_INTERVAL = 0.1

local LEVELS = {
	[Enum.MessageType.MessageOutput] = "print",
	[Enum.MessageType.MessageInfo] = "info",
	[Enum.MessageType.MessageWarning] = "warning",
	[Enum.MessageType.MessageError] = "error",
}

local function post(path, payload)
	payload.token = TOKEN

	return HttpService:RequestAsync({
		Url = BASE_URL .. path,
		Method = "POST",
		Headers = { ["Content-Type"] = "application/json" },
		Body = HttpService:JSONEncode(payload),
	})
end

task.spawn(function()
	-- Studio can finish naming the DataModel after its file a moment after
	-- plugins load, and the check-in below depends on that name.
	for _ = 1, 50 do
		if string.find(game.Name, "%.rbxlx?$") then
			break
		end
		task.wait(0.1)
	end

	-- studio-run only accepts the Studio it launched. Any other Studio that
	-- loads this plugin (or a Studio started after the run ended) gets turned
	-- away here and does nothing.
	--
	-- Studio may not be ready to make requests the moment plugins load, so
	-- connection failures are retried for a few seconds. Any response is final.
	local hello
	for _ = 1, 10 do
		local ok, response = pcall(post, "/hello", {
			placeName = game.Name,
			placeId = game.PlaceId,
		})

		if ok then
			hello = response
			break
		end

		task.wait(0.5)
	end

	if hello == nil or hello.StatusCode ~= 200 then
		return
	end

	local session = HttpService:JSONDecode(hello.Body)

	local queue = {}
	local scriptDone = false
	local result

	local logConnection = LogService.MessageOut:Connect(function(body, messageType)
		-- JSONEncode rejects invalid UTF-8, which would take the whole batch
		-- down with it.
		if utf8.len(body) == nil then
			body = string.gsub(body, "[\128-\255]", "?")
		end

		table.insert(queue, { level = LEVELS[messageType] or "info", body = body })
	end)

	task.spawn(function()
		while true do
			if #queue > 0 then
				local batch = queue
				queue = {}
				pcall(post, "/output", { messages = batch })
			elseif scriptDone then
				break
			end

			task.wait(FLUSH_INTERVAL)
		end

		logConnection:Disconnect()
		pcall(post, "/finish", result)
	end)

	local main = script:FindFirstChildOfClass("ModuleScript")
	local ok, err = xpcall(function()
		return require(main)(plugin, table.unpack(session.args))
	end, function(message)
		return debug.traceback(tostring(message), 2)
	end)

	-- LogService events can be deferred, so give output from the script's last
	-- moments a chance to land in the queue before reporting the result.
	task.wait(FLUSH_INTERVAL * 2)

	result = { success = ok, error = if ok then nil else err }
	scriptDone = true
end)
