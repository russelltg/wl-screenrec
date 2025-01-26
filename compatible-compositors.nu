#!/usr/bin/env nu

let protocols = [[name, url];
	["linux-dmabuf-v1" "https://wayland.app/protocols/linux-dmabuf-v1"]
	["wlr-screencopy-unstable-v1" "https://wayland.app/protocols/wlr-screencopy-unstable-v1"]
	["xdg-output-unstable-v1" "https://wayland.app/protocols/xdg-output-unstable-v1"]
]

let known_compositors = {
	Mutter: "https://mutter.gnome.org/"
	KWin: "https://kde.org/plasma-desktop/"
	Sway: "https://swaywm.org/"
	COSMIC: "https://system76.com/cosmic/"
	Hyprland: "https://hyprland.org/"
	niri: "https://github.com/YaLTeR/niri"
	Weston: "https://wayland.pages.freedesktop.org/weston/"
	Mir: "https://github.com/canonical/mir"
	GameScope: "https://github.com/ValveSoftware/gamescope"
	Jay: "https://github.com/mahkoh/jay"
	Treeland: "https://github.com/linuxdeepin/treeland"
}

$protocols | par-each {|protocol| 
	let html = http get $protocol.url

	let compositors: table<compositor: string, version: string> = $html
		| query web --query "h4#compositor-support + div table th:nth-child(n + 2)"
		| each { |pair| {compositor: ($pair | get 0) version: ($pair | get 1) } }

	let supported: list<bool> = $html
		| query web --query "h4#compositor-support + div table tbody td:nth-child(n + 2)"
		| flatten
		| each { if $in == "x" { false } else { true } }

	$compositors | merge ($supported | wrap $protocol.name)
}
| reduce {|it, acc| $it | join $acc compositor } | reject version_ | reject version_
| insert supported {|row|
	$protocols.name | each { |protocol| $row | get $protocol } | all { $in }
}
| where supported
| select compositor version
| sort-by compositor --ignore-case
| update compositor { |row| $"[($row.compositor)]\(($known_compositors | get $row.compositor )\)" }
| update version { $"`($in)`" }
| to md --pretty
