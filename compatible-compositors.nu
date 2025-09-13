#!/usr/bin/env nu

plugin use query

let and_protocols = [[name, url];
	["linux-dmabuf-v1" "https://wayland.app/protocols/linux-dmabuf-v1"]
	["xdg-output-unstable-v1" "https://wayland.app/protocols/xdg-output-unstable-v1"]
]

let or_protocols = [[name, url];
	["wlr-screencopy-unstable-v1" "https://wayland.app/protocols/wlr-screencopy-unstable-v1"]
	["ext-image-copy-capture-v1" "https://wayland.app/protocols/ext-image-copy-capture-v1"]
]

let protocols = $and_protocols | append $or_protocols

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
	Labwc: "https://labwc.github.io/"
	Treeland: "https://github.com/linuxdeepin/treeland"
	Cage: "https://www.hjdskes.nl/projects/cage/"
	Louvre: "https://github.com/CuarzoSoftware/Louvre"
	Wayfire: "https://wayfire.org/"
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


	let ret = $compositors | merge ($supported | wrap $protocol.name)
	#print $ret
	$ret
}
| reduce {|it, acc| $it | join $acc compositor } | reject version_ | reject version_ # Unclear to me why these `version_` columns are added by `join`
| insert supported {|row|
	let prot_and = $and_protocols.name | each { |protocol| $row | get $protocol } | all { |supported| $supported == true }
	let prot_or = $or_protocols.name | each { |protocol| $row | get $protocol } | any { |supported| $supported == true }
	$prot_and and $prot_or
}
| where supported
| select compositor version
| sort-by compositor --ignore-case
| update compositor { |row|
	let url = try { $known_compositors | get $row.compositor }
	if $url == null {
		error make {msg: $"Unkown compositor ($row.compositor). Wayland Explorer probably got updated with a new compositor. Add it to \$known_compositors in this script"}
		$"($row.compositor)"
	} else {
		$"[($row.compositor)]\(($url)\)"
	}
}
| update version { $"`($in)`" }
| to md --pretty
