-- Builds a Wonder Vision canvas for wv.py. Run in batch mode:
--   aseprite -b --script-param out=X.aseprite --script-param guides=G.png [--script-param from=F.gif] --script wv_canvas.lua
--   out    .aseprite file to write
--   guides PNG overlay at canvas size; goes on a locked "guides" layer that export skips
--   from   optional image/GIF to start from instead of a blank black canvas
local p = app.params
local guides = Image{ fromFile = p.guides }

local spr
if p.from and p.from ~= "" then
  spr = app.open(p.from)
  if spr.colorMode ~= ColorMode.RGB then
    app.command.ChangePixelFormat{ format = "rgb" }
  end
  spr.layers[1].name = "art"
else
  spr = Sprite(guides.width, guides.height, ColorMode.RGB)
  local bg = spr.layers[1]
  bg.name = "bg"
  local fill = Image(spr.width, spr.height, ColorMode.RGB)
  fill:clear(app.pixelColor.rgba(0, 0, 0, 255))
  spr:newCel(bg, 1, fill, Point(0, 0))
  spr:newLayer().name = "art"
end

local g = spr:newLayer()
g.name = "guides"
for _, frame in ipairs(spr.frames) do
  spr:newCel(g, frame, guides, Point(0, 0))
end
g.isEditable = false

spr:saveAs(p.out)
