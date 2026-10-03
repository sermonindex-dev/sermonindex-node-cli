#!/usr/bin/env bash
# SermonIndex node — one-shot machine setup.
#
# For a computer that will sit in a room running the node: an old laptop, a
# mini PC, a Raspberry Pi with a screen. Run it ONCE, as root:
#
#     su -
#     wget -qO- https://sermonindex4.b-cdn.net/node-cli/setup.sh | bash
#
# (`sudo bash` instead of `su -` works too where sudo is set up. wget rather
# than curl because a fresh Debian has wget and not always curl.)
#
# What it does, each step only if needed and safe to repeat:
#
#   1. Installs what the node and its display need (curl, a browser).
#   2. Stops the machine sleeping: suspend, hibernate, and closing the lid.
#   3. Stops the screen blanking or locking: GNOME, XFCE, KDE, LXDE/LXQt,
#      MATE, Cinnamon, Raspberry Pi OS, plain X11, and the text console.
#   4. Adds swap if there is little or none, so a busy node on a small machine
#      slows down instead of being killed.
#   5. Installs the node as a service, running as YOUR account (not root).
#   6. Shows the node's live display full-screen when the desktop starts, and
#      logs in automatically at power-on so it comes back by itself.
#
# One script for every Linux that uses systemd — Debian, Ubuntu, Mint, Raspberry
# Pi OS, Fedora, Arch. Only the package names differ, and the script knows them.
#
# Every question has a default; press Enter to accept it. To run without any
# questions, set them up front, e.g.:
#     SI_USER=greg SI_YES=1 bash setup.sh
#   SI_USER      the account the node runs as        (default: the desktop user)
#   SI_SCOPE     picks | audio | full   (default: nothing chosen; audio/full need approval)
#   SI_STORAGE   library folder                       (default: ~/.sermonindex/downloads)
#   SI_DISPLAY   yes | no  — full-screen node display (default: yes on a desktop)
#   SI_AUTOLOGIN yes | no  — log in at power-on       (default: yes with the display)
#   SI_SWAP      yes | no  — add swap if short        (default: yes)
#   SI_YES=1     accept every default without asking
set -uo pipefail

CDN="https://sermonindex4.b-cdn.net"
c_y=$'\033[1;33m'; c_g=$'\033[1;32m'; c_r=$'\033[1;31m'; c_d=$'\033[2m'; c_0=$'\033[0m'
step(){ printf '\n%s== %s ==%s\n' "$c_y" "$*" "$c_0"; }
ok(){   printf '%s  ✓%s %s\n' "$c_g" "$c_0" "$*"; }
note(){ printf '%s  · %s%s\n' "$c_d" "$*" "$c_0"; }
warn(){ printf '%s  ! %s%s\n' "$c_r" "$*" "$c_0"; }
die(){  printf '\n%s✗ %s%s\n' "$c_r" "$*" "$c_0" >&2; exit 1; }
have(){ command -v "$1" >/dev/null 2>&1; }

# Questions go to the terminal even though the script itself arrives on a pipe.
ask() { # ask "question" default → answer on stdout
  local q="$1" def="$2" a=""
  if [ -n "${SI_YES:-}" ] || [ ! -r /dev/tty ]; then printf '%s' "$def"; return; fi
  printf '%s  ? %s%s [%s] ' "$c_y" "$q" "$c_0" "$def" >/dev/tty
  IFS= read -r a </dev/tty || true
  printf '%s' "${a:-$def}"
}
yes_no() { case "$(ask "$1" "$2")" in [Yy]*) return 0 ;; *) return 1 ;; esac; }

[ "$(id -u)" = "0" ] || die "Run this as root:  su -   (then paste the command again)   or:  sudo bash setup.sh"
have systemctl || die "This machine does not use systemd, which this setup needs."

printf '\n%s  SermonIndex node — machine setup%s\n' "$c_y" "$c_0"
printf '  Turns this computer into a node that runs by itself.\n'

# ── who ──────────────────────────────────────────────────────────────────────
# The desktop user, found the way the system knows it: whoever is logged in on
# the screen, else whoever ran su/sudo, else the first ordinary account.
detect_user() {
  local u=""
  if have loginctl; then
    for s in $(loginctl list-sessions --no-legend 2>/dev/null | awk '{print $1}'); do
      local seat type name
      seat="$(loginctl show-session "$s" -p Seat --value 2>/dev/null)"
      type="$(loginctl show-session "$s" -p Type --value 2>/dev/null)"
      name="$(loginctl show-session "$s" -p Name --value 2>/dev/null)"
      if [ "$seat" = "seat0" ] && { [ "$type" = "x11" ] || [ "$type" = "wayland" ]; } && [ "$name" != "root" ]; then
        u="$name"; break
      fi
    done
  fi
  [ -z "$u" ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ] && u="$SUDO_USER"
  [ -z "$u" ] && u="$(awk -F: '$3>=1000 && $3<60000 && $7 !~ /(nologin|false)$/ {print $1; exit}' /etc/passwd)"
  printf '%s' "$u"
}
NODE_USER="${SI_USER:-$(detect_user)}"
NODE_USER="$(ask "Which account should run the node?" "${NODE_USER:-}")"
[ -n "$NODE_USER" ] && id "$NODE_USER" >/dev/null 2>&1 || die "No account called '$NODE_USER'. Create one first, or run again with SI_USER=name."
[ "$NODE_USER" != "root" ] || die "Pick an ordinary account, not root — the node and its display run as a normal user."
NODE_HOME="$(getent passwd "$NODE_USER" | cut -d: -f6)"
NODE_UID="$(id -u "$NODE_USER")"
ok "the node will run as $NODE_USER ($NODE_HOME)"

GRAPHICAL=no
[ "$(systemctl get-default 2>/dev/null)" = "graphical.target" ] && GRAPHICAL=yes
if have apt-get; then PM=apt; elif have dnf; then PM=dnf; elif have pacman; then PM=pacman; elif have zypper; then PM=zypper; else PM=none; fi
DESKTOP="$( (ls /usr/share/xsessions /usr/share/wayland-sessions 2>/dev/null) | tr 'A-Z' 'a-z' | tr '\n' ' ')"

# ── 1. packages ──────────────────────────────────────────────────────────────
step "1/6  Packages"
pkg_install() {
  case "$PM" in
    apt)    DEBIAN_FRONTEND=noninteractive apt-get install -y -q "$@" >/dev/null ;;
    dnf)    dnf install -y -q "$@" >/dev/null ;;
    pacman) pacman -S --noconfirm --needed "$@" >/dev/null ;;
    zypper) zypper --non-interactive install "$@" >/dev/null ;;
    *)      return 1 ;;
  esac
}
[ "$PM" = apt ] && { apt-get update -q >/dev/null 2>&1 || warn "apt-get update failed — carrying on with what is cached"; }
for p in curl ca-certificates tar; do
  have "${p%%-*}" || pkg_install "$p" || warn "could not install $p"
done
have curl || die "curl is needed and could not be installed."
ok "curl, certificates"

BROWSER="$(command -v chromium || command -v chromium-browser || command -v google-chrome || true)"
WANT_DISPLAY="${SI_DISPLAY:-}"
if [ -z "$WANT_DISPLAY" ]; then
  if [ "$GRAPHICAL" = yes ] && yes_no "Show the node's live display full-screen on this computer's screen?" "Y"; then
    WANT_DISPLAY=yes
  else
    WANT_DISPLAY=no
  fi
fi
if [ "$WANT_DISPLAY" = yes ] && [ -z "$BROWSER" ]; then
  case "$PM" in
    apt)    pkg_install chromium || pkg_install chromium-browser ;;
    dnf)    pkg_install chromium ;;
    pacman) pkg_install chromium ;;
    zypper) pkg_install chromium ;;
  esac
  BROWSER="$(command -v chromium || command -v chromium-browser || true)"
  [ -z "$BROWSER" ] && BROWSER="$(command -v firefox || true)"
  [ -n "$BROWSER" ] && ok "browser for the display: $BROWSER" || { warn "no browser could be installed — skipping the display"; WANT_DISPLAY=no; }
fi

# ── 2. never sleep ───────────────────────────────────────────────────────────
step "2/6  Never sleep"
# Masking the targets is the one switch every desktop respects: whatever a
# power applet decides, the system will not carry it out.
systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target suspend-then-hibernate.target >/dev/null 2>&1
ok "suspend and hibernate disabled"
mkdir -p /etc/systemd/logind.conf.d
cat > /etc/systemd/logind.conf.d/50-sermonindex-node.conf <<'EOF'
# SermonIndex node: a laptop running the node keeps running with its lid shut.
[Login]
HandleLidSwitch=ignore
HandleLidSwitchExternalPower=ignore
HandleLidSwitchDocked=ignore
HandleSuspendKey=ignore
HandleHibernateKey=ignore
IdleAction=ignore
EOF
ok "closing the lid does nothing (takes effect after a restart)"

# ── 3. never blank ───────────────────────────────────────────────────────────
step "3/6  Screen stays on"
as_user() { # run a command in the user's desktop session, if one is running
  local bus="/run/user/$NODE_UID/bus"
  [ -S "$bus" ] || return 1
  runuser -u "$NODE_USER" -- env DBUS_SESSION_BUS_ADDRESS="unix:path=$bus" XDG_RUNTIME_DIR="/run/user/$NODE_UID" "$@" >/dev/null 2>&1
}

# GNOME, and everything built on its settings (Ubuntu, Debian's default,
# Budgie, Cinnamon partly). System-wide defaults via dconf, so it holds even
# for a session that is not running yet — plus the user's own settings now,
# because an existing account's choices win over defaults.
if have dconf || [ -d /etc/dconf ]; then
  mkdir -p /etc/dconf/profile /etc/dconf/db/local.d
  [ -f /etc/dconf/profile/user ] || printf 'user-db:user\nsystem-db:local\n' > /etc/dconf/profile/user
  cat > /etc/dconf/db/local.d/50-sermonindex-node <<'EOF'
[org/gnome/desktop/session]
idle-delay=uint32 0

[org/gnome/desktop/screensaver]
lock-enabled=false
idle-activation-enabled=false

[org/gnome/settings-daemon/plugins/power]
idle-dim=false
sleep-inactive-ac-type='nothing'
sleep-inactive-battery-type='nothing'
power-button-action='nothing'

[org/cinnamon/desktop/session]
idle-delay=uint32 0

[org/cinnamon/desktop/screensaver]
lock-enabled=false

[org/mate/screensaver]
idle-activation-enabled=false
lock-enabled=false

[org/mate/power-manager]
sleep-display-ac=0
sleep-computer-ac=0
EOF
  have dconf && dconf update 2>/dev/null
fi
if have gsettings; then
  as_user gsettings set org.gnome.desktop.session idle-delay 0
  as_user gsettings set org.gnome.desktop.screensaver lock-enabled false
  as_user gsettings set org.gnome.desktop.screensaver idle-activation-enabled false
  as_user gsettings set org.gnome.settings-daemon.plugins.power idle-dim false
  as_user gsettings set org.gnome.settings-daemon.plugins.power sleep-inactive-ac-type nothing
  as_user gsettings set org.gnome.settings-daemon.plugins.power sleep-inactive-battery-type nothing
fi
# XFCE keeps its own settings; set them for the user if a session is up.
if have xfconf-query; then
  for k in blank-on-ac blank-on-battery dpms-on-ac-sleep dpms-on-ac-off dpms-on-battery-sleep dpms-on-battery-off inactivity-on-ac inactivity-on-battery; do
    as_user xfconf-query -c xfce4-power-manager -p "/xfce4-power-manager/$k" -n -t int -s 0
  done
  as_user xfconf-query -c xfce4-power-manager -p /xfce4-power-manager/dpms-enabled -n -t bool -s false
  as_user xfconf-query -c xfce4-screensaver -p /saver/enabled -n -t bool -s false
  as_user xfconf-query -c xfce4-screensaver -p /lock/enabled -n -t bool -s false
fi
# KDE Plasma: no screen lock, no dimming/turning off the screen.
for kw in kwriteconfig6 kwriteconfig5; do
  if have "$kw"; then
    runuser -u "$NODE_USER" -- "$kw" --file kscreenlockerrc --group Daemon --key Autolock false 2>/dev/null
    runuser -u "$NODE_USER" -- "$kw" --file kscreenlockerrc --group Daemon --key LockOnResume false 2>/dev/null
    runuser -u "$NODE_USER" -- "$kw" --file powermanagementprofilesrc --group AC --group DPMSControl --key idleTime 0 2>/dev/null
    runuser -u "$NODE_USER" -- "$kw" --file powerdevilrc --group AC --group Display --key TurnOffDisplayWhenIdle false 2>/dev/null
    runuser -u "$NODE_USER" -- "$kw" --file powerdevilrc --group AC --group Display --key DimDisplayWhenIdle false 2>/dev/null
    break
  fi
done
# Raspberry Pi OS has a switch for exactly this.
have raspi-config && raspi-config nonint do_blanking 1 >/dev/null 2>&1
# Any X11 desktop (LXDE, LXQt, Openbox…): switch off X's own blanking at login.
mkdir -p "$NODE_HOME/.config/autostart"
cat > "$NODE_HOME/.config/autostart/si-noblank.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=SermonIndex: keep the screen on
Exec=sh -c "command -v xset >/dev/null && xset s off -dpms s noblank"
X-GNOME-Autostart-enabled=true
NoDisplay=true
EOF
# The text console (a laptop with no desktop): blank never, from every boot.
cat > /etc/systemd/system/si-console-noblank.service <<'EOF'
[Unit]
Description=SermonIndex node: keep the text console from blanking
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'echo 0 > /sys/module/kernel/parameters/consoleblank 2>/dev/null; for t in /dev/tty1 /dev/tty2; do TERM=linux setterm --blank 0 --powerdown 0 >"$t" <"$t" 2>/dev/null; done; true'
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now si-console-noblank.service >/dev/null 2>&1
ok "no screen blanking, dimming or locking"

# ── 4. swap ──────────────────────────────────────────────────────────────────
step "4/6  Swap"
ram_mb="$(awk '/MemTotal/{print int($2/1024)}' /proc/meminfo)"
swap_mb="$(awk '/SwapTotal/{print int($2/1024)}' /proc/meminfo)"
want_mb=$(( ram_mb < 2048 ? 2048 : (ram_mb > 4096 ? 4096 : ram_mb) ))
note "memory ${ram_mb} MB, swap ${swap_mb} MB"
if [ "$swap_mb" -ge 1024 ]; then
  ok "already has ${swap_mb} MB of swap — leaving it"
elif [ "${SI_SWAP:-yes}" = no ]; then
  note "skipped (SI_SWAP=no)"
elif [ -e /swapfile ]; then
  warn "/swapfile exists but is not in use — leaving it alone; check /etc/fstab"
else
  free_mb="$(df -Pm / | awk 'NR==2{print $4}')"
  fs="$(findmnt -no FSTYPE / 2>/dev/null)"
  if [ "${free_mb:-0}" -lt $(( want_mb + 8192 )) ]; then
    warn "not enough free space on / for ${want_mb} MB of swap — skipped"
  elif [ "$fs" = btrfs ]; then
    if btrfs filesystem mkswapfile --size "${want_mb}m" /swapfile >/dev/null 2>&1; then
      swapon /swapfile && echo '/swapfile none swap sw 0 0' >> /etc/fstab && ok "${want_mb} MB swap file added (btrfs)"
    else
      warn "this btrfs cannot make a swap file here — skipped"
    fi
  else
    if fallocate -l "${want_mb}M" /swapfile 2>/dev/null || dd if=/dev/zero of=/swapfile bs=1M count="$want_mb" status=none; then
      chmod 600 /swapfile && mkswap /swapfile >/dev/null && swapon /swapfile \
        && { grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab; } \
        && ok "${want_mb} MB swap file added, on from every boot"
    else
      warn "could not create /swapfile — skipped"
    fi
  fi
  # Use swap as a cushion, not as working memory.
  echo 'vm.swappiness=10' > /etc/sysctl.d/90-sermonindex-node.conf
  sysctl -q -p /etc/sysctl.d/90-sermonindex-node.conf 2>/dev/null
fi

# ── 5. the node ──────────────────────────────────────────────────────────────
step "5/6  The node"
INSTALL_ENV=(NODE_USER="$NODE_USER" KIOSK=no)
if [ -f /etc/systemd/system/sermonindex-node.service ] && [ -z "${SI_SCOPE:-}${SI_STORAGE:-}" ]; then
  # Already installed: upgrade in place and keep its settings (install.sh does).
  note "already installed — updating it and keeping its settings"
else
  storage="${SI_STORAGE:-$NODE_HOME/.sermonindex/downloads}"
  storage="$(ask "Where should the sermons be stored?" "$storage")"
  mkdir -p "$storage" && chown "$NODE_USER": "$storage" 2>/dev/null
  free_gb="$(df -Pk "$storage" | awk 'NR==2{print int($4/1048576)}')"
  printf '\n  %s GB free there. Nothing downloads until you choose what to hold:\n' "${free_gb:-?}"
  printf '    - pick speakers in the menu (type sermonindex-node), or\n'
  printf '    - hold the whole library as a seed node (about 373 GB of audio, or 2.4 TB with\n'
  printf '      video). That needs approval for this machine: send the request from the\n'
  printf '      menu'"'"'s Seed node page, and it starts by itself once approved.\n'
  if [ -n "${SI_SCOPE:-}" ]; then
    case "$SI_SCOPE" in picks|audio|full) INSTALL_ENV+=(SCOPE="$SI_SCOPE") ;; *) warn "unknown SI_SCOPE '$SI_SCOPE' — ignored" ;; esac
  fi
  [ "$storage" != "$NODE_HOME/.sermonindex/downloads" ] && INSTALL_ENV+=(STORAGE="$storage")
fi
# Open the sharing ports if a firewall is active, so other nodes can reach this one.
if have ufw && ufw status 2>/dev/null | grep -q 'Status: active'; then
  ufw allow 42800:42839/tcp >/dev/null && ufw allow 42800:42839/udp >/dev/null && ok "firewall: sharing ports 42800-42839 opened"
elif have firewall-cmd && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd -q --permanent --add-port=42800-42839/tcp --add-port=42800-42839/udp && firewall-cmd -q --reload && ok "firewall: sharing ports 42800-42839 opened"
fi
curl -fsSL "$CDN/node-cli/install.sh" | env "${INSTALL_ENV[@]}" bash || die "The node installer stopped — the lines above say why."

# ── 6. the display ───────────────────────────────────────────────────────────
step "6/6  Display"
if [ "$WANT_DISPLAY" != yes ]; then
  note "no display (this computer will run the node without showing it)"
  note "the node's page is still at http://localhost:8137/ in any browser here"
else
  mkdir -p "$NODE_HOME/.local/bin" "$NODE_HOME/.config/autostart"
  case "$BROWSER" in
    *firefox*) launch="\"$BROWSER\" --kiosk \"\$URL\"" ;;
    *) launch="\"$BROWSER\" --kiosk --app=\"\$URL\" --user-data-dir=\"\$PROFILE\" --password-store=basic --noerrdialogs --disable-infobars --disable-session-crashed-bubble --no-first-run --disable-pinch --overscroll-history-navigation=0 --disable-features=Translate" ;;
  esac
  cat > "$NODE_HOME/.local/bin/si-kiosk.sh" <<EOF
#!/usr/bin/env bash
# SermonIndex node display — full screen, restarted if it is ever closed.
URL="http://localhost:8137/?view=compact"
PROFILE="\$HOME/.config/si-kiosk-profile"
# Wait for the node to answer, or at boot the page loads before the service is
# up and sits on a connection error.
for i in \$(seq 1 90); do curl -fsS -o /dev/null "\$URL" && break; sleep 2; done
while true; do
  # A power cut leaves the browser's profile looking "in use"; clear the lock.
  rm -f "\$PROFILE"/Singleton* 2>/dev/null
  $launch
  sleep 3
done
EOF
  chmod +x "$NODE_HOME/.local/bin/si-kiosk.sh"
  cat > "$NODE_HOME/.config/autostart/si-node-display.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=SermonIndex Node Display
Exec=$NODE_HOME/.local/bin/si-kiosk.sh
X-GNOME-Autostart-enabled=true
EOF
  rm -f "$NODE_HOME/.config/autostart/si-cog-kiosk.desktop" 2>/dev/null
  ok "display opens full-screen when $NODE_USER's desktop starts (Alt+F4 closes it; it reopens)"

  # Log in by itself at power-on — a display nobody has to sign in to.
  WANT_AUTO="${SI_AUTOLOGIN:-}"
  [ -z "$WANT_AUTO" ] && { yes_no "Log in to $NODE_USER automatically at power-on (so the display comes back by itself)?" "Y" && WANT_AUTO=yes || WANT_AUTO=no; }
  if [ "$WANT_AUTO" = yes ]; then
    done_auto=""
    if [ -f /etc/gdm3/daemon.conf ] || [ -f /etc/gdm3/custom.conf ] || [ -d /etc/gdm3 ]; then
      f=/etc/gdm3/daemon.conf; [ -f /etc/gdm3/custom.conf ] && f=/etc/gdm3/custom.conf
      [ -f "$f" ] || printf '[daemon]\n' > "$f"
      cp -n "$f" "$f.before-sermonindex" 2>/dev/null
      sed -i '/^AutomaticLoginEnable=/d; /^AutomaticLogin=/d' "$f"
      sed -i "/^\[daemon\]/a AutomaticLoginEnable=true\nAutomaticLogin=$NODE_USER" "$f"
      done_auto="GDM"
    elif [ -f /etc/gdm/custom.conf ]; then
      f=/etc/gdm/custom.conf; cp -n "$f" "$f.before-sermonindex" 2>/dev/null
      sed -i '/^AutomaticLoginEnable=/d; /^AutomaticLogin=/d' "$f"
      sed -i "/^\[daemon\]/a AutomaticLoginEnable=true\nAutomaticLogin=$NODE_USER" "$f"
      done_auto="GDM"
    elif [ -d /etc/lightdm ]; then
      mkdir -p /etc/lightdm/lightdm.conf.d
      printf '[Seat:*]\nautologin-user=%s\nautologin-user-timeout=0\n' "$NODE_USER" > /etc/lightdm/lightdm.conf.d/50-sermonindex-node.conf
      getent group autologin >/dev/null && usermod -aG autologin "$NODE_USER"
      done_auto="LightDM"
    elif have raspi-config; then
      raspi-config nonint do_boot_behaviour B4 >/dev/null 2>&1 && done_auto="Raspberry Pi OS"
    elif [ -d /etc/sddm.conf.d ] || [ -f /etc/sddm.conf ]; then
      mkdir -p /etc/sddm.conf.d
      sess="$(ls /usr/share/wayland-sessions /usr/share/xsessions 2>/dev/null | grep -m1 -i plasma | sed 's/\.desktop$//')"
      printf '[Autologin]\nUser=%s\nSession=%s\n' "$NODE_USER" "${sess:-plasma}" > /etc/sddm.conf.d/50-sermonindex-node.conf
      done_auto="SDDM"
    fi
    [ -n "$done_auto" ] && ok "automatic login for $NODE_USER ($done_auto)" || warn "could not find the login manager — turn on automatic login in the system settings"
  fi
fi
# Everything written into the user's home belongs to the user.
chown -R "$NODE_USER": "$NODE_HOME/.config/autostart" "$NODE_HOME/.local/bin" 2>/dev/null
[ -d "$NODE_HOME/.sermonindex" ] && chown -R "$NODE_USER": "$NODE_HOME/.sermonindex" 2>/dev/null

# ── done ─────────────────────────────────────────────────────────────────────
printf '\n%s  Done.%s\n\n' "$c_g" "$c_0"
printf '  Restart once so every setting takes hold:   reboot\n\n'
printf '  After that:\n'
printf '    sermonindex-node            the menu: pick speakers, ask to be a seed node, settings\n'
printf '    sermonindex-node status     what the node is doing\n'
printf '    http://localhost:8137/      the live display, in any browser here\n\n'
printf '  For other nodes to reach this one, forward TCP port 42800 on your router\n'
printf '  to this computer (the menu'"'"'s Node status shows whether it worked).\n\n'
