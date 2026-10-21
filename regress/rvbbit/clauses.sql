-- Scripted clause tails. The rust fake reads the comments; this file is
-- not under regress/rvbbit/sql, so the native rewriter does not see it.
-- Each case is one Reduce over scripted takes. A `-- wave` line starts
-- the next RetryWave. Costs are the wave's USD.

-- case vote
-- wave
-- take model=jev score=0.80 value=true cost=0.001
-- take model=jev score=0.70 value=true cost=0.001
-- take model=other score=0.40 value=false cost=0.001
-- expect winner=true retries=0 identical=false pick=vote
-- sql WITH TAKES 3 PICK BY VOTE

-- case mean-score
-- wave
-- take model=jev score=0.54 value=true cost=0.001
-- take model=other score=0.62 value=false cost=0.001
-- expect winner=true retries=0 identical=false
-- sql WITH TAKES 2 PICK BY MEAN SCORE THRESHOLD 0.5

-- case first-valid
-- wave
-- take model=jev score=0.54 value=aircraft cost=0.001 error
-- take model=jev score=0.61 value=satellite cost=0.001
-- expect winner=satellite retries=0 identical=false
-- sql WITH TAKES 2 PICK BY FIRST VALID

-- case judge
-- wave
-- take model=panel score=0.90 value=aircraft cost=0.001
-- take model=jev score=0.54 value=unexplained cost=0.001 judge
-- expect winner=unexplained retries=0 identical=false
-- sql PICK BY JUDGE

-- case consensus-retry
-- wave
-- take model=jev score=0.54 value=aircraft cost=0.001
-- take model=other score=0.80 value=satellite cost=0.001
-- wave
-- take model=jev score=0.91 value=aircraft cost=0.001
-- take model=other score=0.88 value=aircraft cost=0.001
-- expect winner=aircraft retries=1 identical=false
-- sql WITH TAKES 2 PICK BY CONSENSUS RETRY 1 UNTIL score >= 0.5

-- case identical-takes
-- wave
-- take model=jev score=0.54 value=true cost=0.001
-- take model=jev score=0.54 value=true cost=0.001
-- take model=jev score=0.54 value=true cost=0.001
-- expect winner=true retries=0 identical=true
-- sql WITH TAKES 3 PICK BY MEAN SCORE

-- case fallback
-- wave
-- take model=jev score=0.10 value=true cost=0.001 error
-- wave
-- take model=backup score=0.80 value=true cost=0.002
-- expect winner=true retries=0 identical=false fallback=true
-- sql USING MODEL jev FALLBACK backup PICK BY FIRST VALID

-- case budget-abort
-- wave
-- take model=jev score=0.90 value=true cost=0.010
-- expect error=budget retries=0 identical=false
-- sql WITH TAKES 1 BUDGET 0.001

-- case check-input
-- input-len 0
-- wave
-- take model=jev score=0.90 value=true cost=0.001
-- expect error=check-input retries=0
-- sql CHECK INPUT len >= 1

-- case check-output
-- wave
-- take model=jev score=0.20 value=true cost=0.001
-- take model=jev score=0.80 value=false cost=0.001
-- expect winner=false retries=0 identical=false
-- sql PICK BY FIRST VALID CHECK OUTPUT score >= 0.5
