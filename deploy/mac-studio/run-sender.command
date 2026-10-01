#!/bin/zsh
exec /Users/ajmwagar/.local/bin/isochrone send \
  "Scarlett 2i2 4th Gen" \
  192.168.2.74:50040 \
  0.0.0.0:0 \
  3,4 \
  -6 2>&1 | tee /tmp/isochrone-sender-iterm.log
