-- Studio/Hub -- Tidal boot file for the "orbits" stack.
--
-- Ports are moved out of the defaults to match Hub/startup.scd:
--
--     6110  scsynth      <- oBusPort. Tidal writes control busses DIRECTLY here,
--                           bypassing sclang. Defaults to 57110; if this does not
--                           match the running server the writes vanish silently.
--     6122  SuperDirt    <- oPort, must equal ~dirt.start(6122, ...)
--     6130  Tidal ctrl   <- inbound /ctrl, /mute, /hush, /setcps. Unrelated to audio.
--
-- Boot order is strict: SuperDirt must be up BEFORE Tidal starts. Tidal
-- handshakes once, caches SuperDirt's control bus indices, and never asks
-- again -- so restarting SuperDirt under a live Tidal leaves Tidal writing to
-- the dead instance's busses. Restart Tidal after any server restart.

:set -fno-warn-orphans -Wno-type-defaults -XMultiParamTypeClasses -XOverloadedStrings
:set prompt ""

import Sound.Tidal.Boot

default (Rational, Integer, Double, Pattern String)

-- ghci evaluates one line at a time, so anything multi-line needs :{ ... :}

:{
let hubTarget = superdirtTarget
      { oLatency = 0.05
      , oAddress = "127.0.0.1"
      , oPort    = 6122
      , oBusPort = Just 6110
      }
:}

:{
let hubConfig = defaultConfig
      { cVerbose       = True
      , cFrameTimespan = 1 / 20
      , cCtrlAddr      = "127.0.0.1"
      , cCtrlPort      = 6130
      }
:}

tidalInst <- mkTidalWith [(hubTarget, [superdirtShape])] hubConfig

-- This orphan instance is what makes d1 .. d16, hush, p, setcps etc. resolve.
-- It has to come after tidalInst is defined.
instance Tidally where tidal = tidalInst

:set prompt "tidal> "
:set prompt-cont ""
