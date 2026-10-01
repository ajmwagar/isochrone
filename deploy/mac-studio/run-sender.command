#!/bin/zsh
exec /Users/ajmwagar/.local/bin/isochrone send \
  "BlackHole 2ch" \
  192.168.2.74:50040 \
  0.0.0.0:0 \
  1,2 \
  -6 2>&1 | tee /tmp/isochrone-sender-iterm.log
