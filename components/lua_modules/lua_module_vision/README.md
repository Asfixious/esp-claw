# lua_module_vision

Lua vision modules backed by `image.frame` buffers.

## Modules

- `motion_detect`: detects local motion in consecutive frames. It is enabled by default with `LUA_MODULE_VISION_MOTION_DETECT`.
- `espdet`: runs ESP-DL ESPDet object detection with a user-provided `.espdl` model. Enable it with `LUA_MODULE_VISION_ESPDET`.

All functions borrow frame data only during the call. Frame-format conversion is handled by the shared `image` module.

## Motion Detection

The detector compares RGB565 luma against the previous frame inside an ROI, groups changed pixels into blocks, pads and smooths the resulting box, and uses confirm/hold state to produce a stable alert.

```lua
local camera = require("camera")
local motion = require("motion_detect")

local detector = motion.new({
    roi = { x = 0, y = 40, width = 240, height = 180 },
    pixel_diff_threshold = 24,
    active_pixel_percent = 5,
    confirm_frames = 2,
    hold_frames = 3,
})

local frame <close> = camera.get_frame(3000)
local result = detector:detect(frame)

if result.motion and result.box then
    print("motion box", result.box.left, result.box.top,
          result.box.right, result.box.bottom)
end
```

The first `detect()` call seeds the previous-frame buffer and returns `ready = false`; subsequent calls compare against the preceding frame.

### API

- `motion.new([opts]) -> detector`: creates an independent detector.
- `detector:detect(frame) -> result`: runs detection with the detector's fixed configuration.
- `detector:reset()`: clears previous-frame and alert state.
- `detector:close()`: releases working buffers early. Garbage collection also releases them.

### Options

- `roi`: `{ x, y, width, height }`, defaulting to the whole frame.
- `pixel_diff_threshold`: luma-difference threshold, default `24`.
- `active_pixel_percent`: active ROI percentage required for raw detection, default `5`.
- `confirm_frames`: consecutive positive frames required to activate, default `2`.
- `hold_frames`: frames to hold an alert after raw detection clears, default `3`.
- `block_size`: motion-block edge length, default `4`.
- `block_hit_pixels`: changed pixels required to activate a block, default `12`.
- `box_padding`: padding around the raw motion box, default `2`.
- `box_deadband`: smoothed-edge changes to ignore, default `2`.
- `box_snap_threshold`: edge distance that snaps immediately, default `24`.

Results include `ready`, `motion`, `event`, `score`, and the smoothed display `box` when motion is active. Events are `"none"`, `"started"`, or `"stopped"`.

## ESPDet

```lua
local espdet = require("espdet")
local image = require("image")
local storage = require("storage")

local root = storage.get_root_dir()
local model_path = storage.join_path(root, "test", "espdet_pico_224_224_cat.espdl")
local image_path = storage.join_path(root, "test", "cat.jpg")

espdet.load(model_path, { score_threshold = 0.6 })
local source <close> = image.load_file(image_path)
local result = espdet.detect(source, { score_threshold = 0.6 })
print("detection count=" .. tostring(result.count))
espdet.unload()
```

`espdet.detect(frame, opts)` requests RGB565LE through the image module. Load a model once with `espdet.load(path[, opts])`, or pass `opts.model_path` to a detect call. Results include `count`; each detection includes `category`, `score`, `box`, `left`, `top`, `right`, `bottom`, `x`, `y`, `width`, and `height`.
