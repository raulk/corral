source_root = defines.get("source_root", "target/dmg-root")

format = defines.get("format", "UDZO")
background = "docs/assets/corral-dmg-background.png"
window_rect = ((160, 120), (660, 420))
icon_size = 96
text_size = 13
show_icon_preview = True

files = [
    f"{source_root}/Corral.app",
]

symlinks = {
    "Applications": "/Applications",
}

icon_locations = {
    "Corral.app": (170, 230),
    "Applications": (490, 230),
}
