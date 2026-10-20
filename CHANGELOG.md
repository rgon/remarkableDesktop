# Changelog

## [0.11.6](https://github.com/rgon/remarkableDesktop/compare/v0.11.5...v0.11.6) (2026-10-20)


### Features

* fill dark pixels in edge mode with a GPU threshold ([0705807](https://github.com/rgon/remarkableDesktop/commit/0705807ba0185f6974290fd8cd3b8c3c62f475c8))
* fill enclosed dark regions in edge mode ([eabfe52](https://github.com/rgon/remarkableDesktop/commit/eabfe5291907c0f00c09d2a2775297f22af953e6))
* find the changed tiles of edge frames on the GPU ([1543de8](https://github.com/rgon/remarkableDesktop/commit/1543de8c0d6bef41d9013f028a35229600063a75))
* run edge detection on the GPU through Vulkan with a CPU fallback ([d2feec5](https://github.com/rgon/remarkableDesktop/commit/d2feec5b5c33060a547f58d6231a250cecd88b6f))


### Performance Improvements

* pack edge frames to bits on the GPU and send them without a 4-bit copy ([94d26f5](https://github.com/rgon/remarkableDesktop/commit/94d26f55cb2cfceb6c9060607e8502f6093afd69))
* speed up edge mode and skip stale capture frames ([ce45777](https://github.com/rgon/remarkableDesktop/commit/ce45777a17f00bed5fba8cb6e5ddeca674c796c0))

## [0.11.5](https://github.com/rgon/remarkableDesktop/compare/v0.11.4...v0.11.5) (2026-10-20)


### Features

* drop the "remote controlling" banner from the tablet's screen ([ba5c02e](https://github.com/rgon/remarkableDesktop/commit/ba5c02ecd744ce11cf07e4ba2164823605603d11))
* host-driven tile updates, settle refresh and 1-bit edge frames ([d6f863d](https://github.com/rgon/remarkableDesktop/commit/d6f863deb3f08d72f2b572134f9979239adc2780))
* reject a resting palm in touchpad mode when another finger lands ([c88d0f0](https://github.com/rgon/remarkableDesktop/commit/c88d0f034f19b4c3bf816e82a4d533c762405996))


### Bug Fixes

* draw edges one pixel wide on the darker side ([c876d90](https://github.com/rgon/remarkableDesktop/commit/c876d90c6c742d340653746c5907bf4a1eda23e0))


### Performance Improvements

* cut input latency and send pen and touch over UDP on Wi-Fi ([fc6c5b6](https://github.com/rgon/remarkableDesktop/commit/fc6c5b6494892eec3a0d6fe3ac09a97214a26b51))

## [0.11.4](https://github.com/rgon/remarkableDesktop/compare/v0.11.3...v0.11.4) (2026-10-20)


### Features

* reconnect over usb or wifi, update outdated tablets and warn when uinput is not writable ([4549c9d](https://github.com/rgon/remarkableDesktop/commit/4549c9d9043c199391e5c080bc1f353690ae1d33))
* right-align the wifi address on the tablet's HOST row ([00e5a2e](https://github.com/rgon/remarkableDesktop/commit/00e5a2e3930f2377f62412c075dcc41d28d487fe))
* show the battery percentage on the tablet's version line ([07e4898](https://github.com/rgon/remarkableDesktop/commit/07e4898f786491c85c98a8efd4b5a5b922d188a9))
* show the tablet's wifi address in the info box ([9f13c3a](https://github.com/rgon/remarkableDesktop/commit/9f13c3a4343c58a43ee87fb4efa979a4a4759e5b))


### Bug Fixes

* reuse the host's ssh password in install.sh instead of asking again ([a12ff7a](https://github.com/rgon/remarkableDesktop/commit/a12ff7aab912cadd9e03d4fcf72d66104e801fb5))


### Performance Improvements

* build the release profile with opt-level 3 ([332e307](https://github.com/rgon/remarkableDesktop/commit/332e3072056bf983105b23a2a52cf42b077426da))
* track pen ink with a bitmask so streamed updates no longer scan every inked pixel ([80b220d](https://github.com/rgon/remarkableDesktop/commit/80b220d8f146d05a910649d5f4d2fbd0aa65bbf7))

## [0.11.3](https://github.com/rgon/remarkableDesktop/compare/v0.11.2...v0.11.3) (2026-10-19)


### Features

* add a pen ink preview with INK and CLR buttons and a display mode button on the tablet ([7a98790](https://github.com/rgon/remarkableDesktop/commit/7a987906b295f52af01dbe73058a2e422ae3de25))
* ask for the tablet's ssh password when key login is refused in run.sh and install.sh ([c11f782](https://github.com/rgon/remarkableDesktop/commit/c11f78253562175bd5652515004d7a7f00802785))
* build tablet binaries in CI and bundle an install command in the AppImage ([2bf889a](https://github.com/rgon/remarkableDesktop/commit/2bf889a806cb26d5584e8d7788f5c17a777d4b58))
* stream display modes (normal, off, edge outlines) and ask for the ssh password in the host ([4d9e200](https://github.com/rgon/remarkableDesktop/commit/4d9e20034101faa0c64ab9dda90af82857173ee8))


### Bug Fixes

* set ARCH for appimagetool so the bundled tablet binaries don't confuse it ([732ab0d](https://github.com/rgon/remarkableDesktop/commit/732ab0dbb290eb32311c54c1dc512f0369f636fd))

## [0.11.2](https://github.com/rgon/remarkableDesktop/compare/v0.11.1...v0.11.2) (2026-10-08)


### Features

* aarch64 tablet target for the paper pro and runtime qt version detection (v0.11.1) ([dd14ea3](https://github.com/rgon/remarkableDesktop/commit/dd14ea39ca5ea4b028db80db80c0264caf62c4ea))
* add palm rejection, latency stats and kernel timestamps (v0.2.0) ([f74bcca](https://github.com/rgon/remarkableDesktop/commit/f74bccae9d181eaccd49800d09f597468e73f437))
* add pen mouse mode with hold button for apps without tablet cursors (v0.4.0) ([12a636f](https://github.com/rgon/remarkableDesktop/commit/12a636fdfbf01a2edebd6ec404e0ca8d8c36937f))
* go-remote button to move the host link off usb once reachable (v0.7.0) ([8affb14](https://github.com/rgon/remarkableDesktop/commit/8affb1431f4dc4e7fae0dbee3bf548ab41a409e3))
* **host:** attach to the tablet instance, reconnect after usb re-enumeration, map hid devices (v0.6.0) ([ac9ee4f](https://github.com/rgon/remarkableDesktop/commit/ac9ee4fc4c2590cdd04d60055f32fc50679f9ef9))
* **host:** capture monitors or a virtual monitor via mutter screencast and gstreamer ([ec177a4](https://github.com/rgon/remarkableDesktop/commit/ec177a4d74257ad92e69373418f16773cfd2d073))
* **host:** direct tcp_nodelay link, per-monitor mapping and device replacement ([c3fd0e4](https://github.com/rgon/remarkableDesktop/commit/c3fd0e46da3473777aaff7c7303bbf52c9917dd0))
* **host:** replay tablet input through uinput devices over ssh ([03d2a30](https://github.com/rgon/remarkableDesktop/commit/03d2a30ff5febc13d5ad3169140c710f846f2424))
* **host:** stream virtual or mirrored monitor to the tablet and keep monitor layout (v0.5.0) ([3b444ac](https://github.com/rgon/remarkableDesktop/commit/3b444ac5db426bd1df742b5f5ef295301012b853))
* measure usb hid pen latency with tablet markers, shutdown logging (v0.6.1) ([6473009](https://github.com/rgon/remarkableDesktop/commit/64730096eb8a531a8eec742c8b53e5a150610047))
* **proto:** add 4-bit gray rect updates with acks (protocol v4) ([8206c66](https://github.com/rgon/remarkableDesktop/commit/8206c66546cf907938f3038e13d73a3c76242af9))
* **proto:** add direct link, screen list and target messages (protocol v3) ([14dd4ba](https://github.com/rgon/remarkableDesktop/commit/14dd4ba09ebbb70e713770dc510bb38d333ff564))
* **proto:** add tablet-to-host wire format for virtual input devices ([0eff094](https://github.com/rgon/remarkableDesktop/commit/0eff094c8c147207cf6335886d45530c755d4c31))
* **proto:** hello carries the tablet's selected view (protocol v7) ([97a6b9f](https://github.com/rgon/remarkableDesktop/commit/97a6b9f126909e22f4a0061507a557c8631ff5da))
* **proto:** keys message with the host layout's undo and redo keys (protocol v8) ([8c5b1f7](https://github.com/rgon/remarkableDesktop/commit/8c5b1f7b83cf401bd754c0b040863b216b1223eb))
* **proto:** mapping and status messages for usb hid sessions (protocol v5) ([5d78769](https://github.com/rgon/remarkableDesktop/commit/5d78769aa507b2e8ffc4561a4c56f9a00915459f))
* **proto:** tablet addresses, remote reachability and go-remote messages (protocol v6) ([98cc27d](https://github.com/rgon/remarkableDesktop/commit/98cc27df0a6b3d13778910f8b2c9141c35328a16))
* remember view, pen and touch modes across reconnects and restarts (v0.8.0) ([7f65837](https://github.com/rgon/remarkableDesktop/commit/7f65837b892c73e61c9b5cda760e1c2614ed9ac2))
* run.sh checks uinput access and offers to install the udev rule ([9cdd03d](https://github.com/rgon/remarkableDesktop/commit/9cdd03db1925ff87708df61334b44cb83b4199dd))
* run.sh launcher with one-shot tablet start and separate client (v0.7.1) ([34e395b](https://github.com/rgon/remarkableDesktop/commit/34e395bc59ed6106bde266e81fb4073c983be552))
* **tablet:** add probe and restore subcommands ([73f2a53](https://github.com/rgon/remarkableDesktop/commit/73f2a539c9175d30992b92f3dde9be3b8cc16be1))
* **tablet:** add run mode forwarding pen and touch with hold-to-close ui ([7c9be43](https://github.com/rgon/remarkableDesktop/commit/7c9be4338d7a72cbeed3fb25af160397317bfa93))
* **tablet:** add touch mode and screen hold buttons and direct low-latency link (v0.3.0) ([9aadab1](https://github.com/rgon/remarkableDesktop/commit/9aadab19112342f73b3cffbe681cb5fd916ac1d3))
* **tablet:** drive rm2 e-ink engine via dlopen and embedded-graphics canvas ([dc4a29f](https://github.com/rgon/remarkableDesktop/commit/dc4a29f4a0aa294a2ed582e4fd5a4e6d2507c045))
* **tablet:** drop the hold-a-button hint from the home screen ([8bbaf0a](https://github.com/rgon/remarkableDesktop/commit/8bbaf0a27f14a1c85dc33d4d330b8fd994a5ab96))
* **tablet:** functionfs usb hid gadget prototype next to the network functions ([61aab6e](https://github.com/rgon/remarkableDesktop/commit/61aab6ef7472c063999a33053390c9aeca7cabad))
* **tablet:** keep the pen off the computer over the buttons and let it press them (v0.11.0) ([e7b3400](https://github.com/rgon/remarkableDesktop/commit/e7b3400abfec46a2a07287a22dd7b45a750ef743))
* **tablet:** pause xochitl with guardian, watchdog petting and input grabs ([444e43a](https://github.com/rgon/remarkableDesktop/commit/444e43a5b4503a2eed890d79cd533db3a3354e68))
* **tablet:** show host screen updates with fast then quality refresh ([7785feb](https://github.com/rgon/remarkableDesktop/commit/7785feb315e098fc67c19ea0528ae552fa09c4a5))
* **tablet:** show version on screen (v0.4.2) ([a66fb11](https://github.com/rgon/remarkableDesktop/commit/a66fb11b3ec144abdb1683f6add7996b872e3f9c))
* **tablet:** smaller hold buttons ([69e9e87](https://github.com/rgon/remarkableDesktop/commit/69e9e87ed030490f3a5dd92a590d5297326cf265))
* **tablet:** tablet-owned instance with usb hid input, info box, greyed host-only options and side button ([151b0be](https://github.com/rgon/remarkableDesktop/commit/151b0be4598261970409ac6a49e499ab11d1330e))
* undo and redo buttons sending ctrl+z and ctrl+y over usb hid or uinput (v0.9.0) ([04f5bec](https://github.com/rgon/remarkableDesktop/commit/04f5bec15d702a5afe0d8701ef5210dbe8f43c02))
* undo/redo send dedicated keys by default and ctrl+layout keys when the host detects the layout (v0.10.0) ([19ef62d](https://github.com/rgon/remarkableDesktop/commit/19ef62d2234753f51814c662df4c2d15766b5f99))


### Bug Fixes

* detect dead host links within 1.5 s and never block input on sends ([cf3dc0b](https://github.com/rgon/remarkableDesktop/commit/cf3dc0bd20be9ca5fdcdf5437b13c6cebfddc91e))
* **host:** log unreachable tablet addresses once known ([9e1874c](https://github.com/rgon/remarkableDesktop/commit/9e1874c3dc0a18c678584ac7d28e85b5efaca4aa))
* **host:** map pen and touchscreen via gnome device settings, keep aspect, white letterbox (v0.5.3) ([537fb18](https://github.com/rgon/remarkableDesktop/commit/537fb1838d184d5d00090aa94887859366b6cd14))
* **host:** ssh keepalives so calls caught by usb re-enumeration fail fast ([a349f0f](https://github.com/rgon/remarkableDesktop/commit/a349f0fed481740165b84de16d969d6780225ccd))
* **proto:** parse older hellos so version mismatches are reported ([197b3ba](https://github.com/rgon/remarkableDesktop/commit/197b3ba25fa8602cfc4e68cacb1eeaab776e8c54))
* run.sh help prints exactly the header comment ([cc1ed88](https://github.com/rgon/remarkableDesktop/commit/cc1ed886a8900ebedb3860d66990db85a8987691))
* **tablet:** draw into rgb32 buffer and restore xochitl screen from its qimage ([13030d0](https://github.com/rgon/remarkableDesktop/commit/13030d046942c1b1b0cf0a5ab61bac23284b60a5))
* **tablet:** match xochitl's frame by qimage format to avoid ambiguous restores ([03cc9a4](https://github.com/rgon/remarkableDesktop/commit/03cc9a4a5468dc68451060de67899742959673f8))
* **tablet:** resume xochitl when its candidate frames are identical ([c8f9ea5](https://github.com/rgon/remarkableDesktop/commit/c8f9ea590715cba6a7787eb4e8f57267e47d3f46))
* **tablet:** run guardian as transient systemd unit and never panic on closed stderr ([e2b37c8](https://github.com/rgon/remarkableDesktop/commit/e2b37c8891c28e94ebdf609b249f18bc6029fcd4))
* **tablet:** service the engine's qt events after every refresh to avoid stalled updates ([6d888e5](https://github.com/rgon/remarkableDesktop/commit/6d888e54731d9498e42a5b6778d9b915543f7095))
* **tablet:** take termination signals back from the e-ink engine and disable core dumps ([50f6dd4](https://github.com/rgon/remarkableDesktop/commit/50f6dd4c46c2a420a8c0c97f4efd9d5a843fb0a5))


### Performance Improvements

* pen fast mode, non-flashing settle, early acks, leaky capture queues and display latency stats (v0.5.2) ([eae1ad8](https://github.com/rgon/remarkableDesktop/commit/eae1ad87d42abe395545758accd6775d76aed2ae))
* **tablet:** decode screen updates on a normal-priority reader thread (v0.5.1) ([33c9a09](https://github.com/rgon/remarkableDesktop/commit/33c9a095b9bbe86230c6028fcae263f595d968ee))
