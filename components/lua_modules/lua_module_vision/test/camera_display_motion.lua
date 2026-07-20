local board_manager = require("board_manager")
local camera = require("camera")
local delay = require("delay")
local display = require("display")
local image = require("image")
local motion = require("motion_detect")

local TAG = "[camera_display_motion]"
local RUN_SECONDS = 30
local CAPTURE_TIMEOUT_MS = 3000
local FRAME_INTERVAL_MS = 30
-- Whatever the sensor exposes; image.convert covers any of these on output.
local CAMERA_OPEN_OPTS = { format = { "JPEG", "RGBP", "YUYV", "UYVY", "YU12" }, width = 320, height = 240, nearest = true, }
local MOTION_OPTS = {
    pixel_diff_threshold = 24,
    active_pixel_percent = 5,
    confirm_frames = 2,
    hold_frames = 3,
    block_size = 4,
    block_hit_pixels = 15,
}

local display_started = false
local camera_started = false
local detector = motion.new(MOTION_OPTS)

local function cleanup()
    if display_started then
        pcall(display.end_frame)
        pcall(display.deinit)
        display_started = false
    end
    if camera_started then
        pcall(camera.close)
        camera_started = false
    end
end

local function draw_result_overlay(frame_index, remaining_s, detect_result)
    local has_previous = detect_result.has_previous == true
    local active_pixels = detect_result.active_pixels or 0
    local moved = detect_result.alert_active == true
    local status = moved and "MOTION" or (has_previous and "STILL" or "WARMUP")
    local bg = { r = 24, g = 24, b = 24 }

    if moved then
        bg = { r = 160, g = 24, b = 24 }
    elseif has_previous then
        bg = { r = 24, g = 96, b = 48 }
    end

    -- Draw an ASCII status bar after the camera preview so detection result is visible on screen.
    display.fill_rect(0, 0, display.width, 48, bg)
    display.draw_text(8, 6, string.format("motion: %s", status), {
        color = "white",
        font_size = 16,
    })
    display.draw_text(8, 28, string.format("pixels=%d frame=%d left=%ds", active_pixels, frame_index, remaining_s), {
        color = "white",
        font_size = 12,
    })
end

local panel_handle, io_handle, lcd_width, lcd_height, panel_if = board_manager.get_display_lcd_params("display_lcd")
if not panel_handle then
    print(TAG .. " SKIP: get_display_lcd_params failed: " .. tostring(io_handle))
    return
end

local camera_paths, path_err = board_manager.get_camera_paths()
if not camera_paths then
    print(TAG .. " SKIP: get_camera_paths failed: " .. tostring(path_err))
    return
end

local ok, err = pcall(display.init, panel_handle, io_handle, lcd_width, lcd_height, panel_if)
if not ok then
    print(TAG .. " SKIP: display.init failed: " .. tostring(err))
    return
end
display_started = true

ok, err = pcall(camera.open, camera_paths.dev_path, CAMERA_OPEN_OPTS)
if not ok then
    print(TAG .. " SKIP: camera.open failed: " .. tostring(err))
    cleanup()
    return
end
camera_started = true
print(string.format("%s using format=%s", TAG, camera.info().pixel_format))

local run_ok, run_err = xpcall(function()
    local stream = camera.info()
    local start_s = os.time()
    local deadline_s = start_s + RUN_SECONDS
    local frames = 0
    local moved_frames = 0

    print(string.format("%s start %ds stream=%dx%d format=%s",
        TAG, RUN_SECONDS, stream.width, stream.height, tostring(stream.pixel_format)))

    while os.time() < deadline_s do
        local now_s = os.time()
        local remaining_s = deadline_s - now_s
        local frame <close> = camera.get_frame(CAPTURE_TIMEOUT_MS)
        local rgb565 <close> = image.convert(frame, image.RGB565)
        local detect_result = detector:detect(rgb565)

        frames = frames + 1
        if detect_result.alert_active == true then
            moved_frames = moved_frames + 1
        end

        display.begin_frame({ clear = true, color = "black" })
        display.draw_image(0, 0, rgb565, {
            mode = "fit",
            width = display.width,
            height = display.height,
        })
        draw_result_overlay(frames, remaining_s, detect_result)
        display.present()
        display.end_frame()

        if frames == 1 or frames % 15 == 0 then
            print(string.format("%s frame=%d detected=%s alert=%s active_pixels=%s event=%s moved_frames=%d",
                TAG, frames, tostring(detect_result.detected), tostring(detect_result.alert_active),
                tostring(detect_result.active_pixels), tostring(detect_result.event), moved_frames))
        end

        if FRAME_INTERVAL_MS > 0 then
            delay.delay_ms(FRAME_INTERVAL_MS)
        end
    end

    display.begin_frame({ clear = true, color = "black" })
    display.draw_text_aligned(0, 0, display.width, display.height, string.format("Motion test done\nframes=%d moved=%d", frames, moved_frames), {
        color = "white",
        font_size = 20,
        align = "center",
        valign = "middle",
    })
    display.present()
    display.end_frame()

    print(string.format("%s PASS frames=%d moved_frames=%d", TAG, frames, moved_frames))
end, debug.traceback)

cleanup()
detector:close()

if not run_ok then
    error(run_err)
end
