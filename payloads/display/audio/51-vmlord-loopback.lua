-- Shipped at /etc/wireplumber/main.lua.d/51-vmlord-loopback.lua
-- For WirePlumber 0.4, which reads Lua rather than SPA-JSON.
--
-- The same rule as 51-vmlord-loopback.conf: the loopback's capture side is
-- what vmlord-display-audio reads, and it is not a microphone.
table.insert(alsa_monitor.rules, {
  matches = {
    {
      { "node.name", "matches", "alsa_input.platform-snd_aloop.*" },
    },
  },
  apply_properties = {
    ["node.disabled"] = true,
  },
})
