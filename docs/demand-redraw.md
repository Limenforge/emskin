# Demand-driven redraw on Wayland

This branch is based on `feat/spring-jelly-cursor` at
`8454850edb05fe50db7cd8d5a3dea062e2960c1d`.

The compositor previously requested another winit redraw from every redraw
callback. It also marked the scene dirty after every Wayland client dispatch
and every IPC poll, even when no visual state changed. This could keep GPU
work running while Emacs was focused but idle.

Now surface commits, nonempty IPC batches, input, and active effects request
frames. The calloop tick asks winit for a redraw only while the scene is
dirty. Jelly cursor animation still requests another frame while its spring
is moving.

Smithay's winit event loop also needs `ControlFlow::Wait` here. With its
original `ControlFlow::Poll`, calloop spins at nearly one CPU core when
redraws become sparse. `vendor/smithay` is the MIT-licensed
[`emskin/smithay`](https://github.com/emskin/smithay) source at
`4d3e1c86f1b0a4fb7df876db0af849f4bba06f88`, with that one backend
change in `src/backend/winit/mod.rs`. Vendoring keeps an ordinary
`cargo build --release -p emskin` reproducible without a second fork or a
machine-specific path.

The vendor copy contains Smithay's library, examples, and benchmarks; its
unrelated sibling workspace members are omitted, so the vendored manifest
lists no additional workspace members.

On one Linux/Hyprland machine, focused idle emskin usage fell from roughly
3% CPU and 10% GPU to approximately zero. Under a timed Emacs `-Q` text
insertion workload, emskin GPU usage fell from about 16% to 4%. Screenshot
and recording integration tests passed; Doom Emacs started and connected to
IPC. The timer workload does not drive Emacs `post-command-hook`, so it does
not measure the complete jelly animation. Sustained Doom insertion still
added significant CPU and heat compared with native Doom; this change does
not remove the cost of nested compositing.

Synthetic `wtype` keystrokes arrived as Escape events inside emskin on that
machine, so those samples were excluded from input-performance conclusions.
No Hyprland, launcher, or Doom configuration is part of this branch.
