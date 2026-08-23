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

-- Orbit-named aliases matching the groups in startup.scd:
--   b1..b6 -> orbits 0-5    (beats)
--   l1..l6 -> orbits 6-11   (leads)
--   a1..a6 -> orbits 12-17  (ambients)
--
-- `|<` takes values from the left, so the orbit acts as a default that a
-- pattern can still override with its own `# orbit n`. Growing to 4 groups of
-- 9 means extending these lists to match ~hubGroups / ~hubPerGroup.
:{
let b1 pat = p "b1" $ pat |< orbit 0
    b2 pat = p "b2" $ pat |< orbit 1
    b3 pat = p "b3" $ pat |< orbit 2
    b4 pat = p "b4" $ pat |< orbit 3
    b5 pat = p "b5" $ pat |< orbit 4
    b6 pat = p "b6" $ pat |< orbit 5
    l1 pat = p "l1" $ pat |< orbit 6
    l2 pat = p "l2" $ pat |< orbit 7
    l3 pat = p "l3" $ pat |< orbit 8
    l4 pat = p "l4" $ pat |< orbit 9
    l5 pat = p "l5" $ pat |< orbit 10
    l6 pat = p "l6" $ pat |< orbit 11
    a1 pat = p "a1" $ pat |< orbit 12
    a2 pat = p "a2" $ pat |< orbit 13
    a3 pat = p "a3" $ pat |< orbit 14
    a4 pat = p "a4" $ pat |< orbit 15
    a5 pat = p "a5" $ pat |< orbit 16
    a6 pat = p "a6" $ pat |< orbit 17
:}

:{
let steel = "0.2 1 1"
    gain_slide x y z = gain y
    ray = "<[vt:2 vt:2 vt:2 [vt:2*2] ~ exxo:3 ], vt:3*4, [vt:3 ~ vt:3*2]>"
:}

:set prompt "tidal> "
:set prompt-cont ""
