# life — a deterministic headless ecosystem

`life` runs a small world forward one tick at a time. Every creature is shown
a bounded observation and answers with exactly one intent; the world carries
out the intents in creature-id order, resolves the conflicts between them,
grows some food, and hands back the next world. Nothing is drawn and nothing
is timed: a run reads its arguments, prints its lines, and asks a host for
nothing else.

It is [issue #91](https://github.com/myuon/cove/issues/91)'s V0 model, at the
size a test can afford, and it is here to answer two questions the other
examples do not:

- **What does a copy mean when the program is a simulation?** `examples/values`
  demonstrates that a struct copies and a `Vector` aliases. This is the same
  fact load-bearing: the world is a value because it holds no `Vector`, and
  the resolution loop works because a `Vector` handed to a helper *is* the
  loop's own vector.
- **Is a deterministic run something a language can make easy?** Cove has no
  random source, so the honest answer is a generator written in Cove, and
  determinism is then a property of the program rather than a promise about
  the machine.

## Running it

```console
$ cd examples
$ cove run life
cove-life: seed 7, 12 tick(s) over 12x8 cells
tick     0  alive   8  forager  4  predator  2  scavenger  2  food  204  energy   112  hash 902111605
tick     4  alive   7  forager  3  predator  2  scavenger  2  food  205  energy   166  hash 1549081087
tick     8  alive  13  forager  7  predator  2  scavenger  4  food  204  energy   240  hash 990376900
tick    12  alive  13  forager  7  predator  2  scavenger  4  food  191  energy   334  hash 1234420474
cove-life: 12 tick(s), 7 birth(s), 2 death(s), 0 refusal(s), hash 1234420474
```

That is the whole default: twelve ticks over ninety-six cells. It is small
because this entry is in the differential corpus, which runs it on both
backends on every push, and a corpus case's size is a promise about somebody
else's time. Everything interesting is an argument:

```console
$ cove run life -- --seed 42 --ticks 10000 --every 2500
$ cove run life -- --width 16 --height 10 --ticks 600 --every 100
$ cove run life -- --ticks 40 --inspect
$ cove run life --files-root . -- --ticks 100 --journal run-7.jsonl
```

`--journal` writes one JSON Lines record per reported tick, which is what a
replay reads:

```text
{"tick":0,"hash":902111605,"alive":8,"foragers":4,"predators":2,"scavengers":2,"food":204,"energy":112,"births":0,"deaths":0,"refusals":0}
{"tick":4,"hash":1549081087,"alive":7,"foragers":3,"predators":2,"scavengers":2,"food":205,"energy":166,"births":0,"deaths":1,"refusals":0}
```

`cove run life -- --help` lists the options.

## The model

A grid of cells, each holding food from 0 to 4. A population of creatures,
each with an id, a species, a cell, and an energy. A tick is:

1. **Every creature decides.** It is shown an `Observation` — the food under
   it, the four cells around it, the nearest few creatures it can see, and,
   for a scavenger, the way to the most food within its range. It answers one
   `Decision`: `Move`, `Eat`, `Hunt`, `Hide`, or `Rest`.
2. **The world resolves.** In creature-id order: a step onto a cell somebody
   already holds is `Blocked`, a step off the grid is `Refused`, a meal in an
   empty cell is `Refused`, a hunt of a creature that is not there is
   `Refused`, and a hunt into a thicket is `Refused`. What each intent came to
   is an `ActionResult`.
3. **The world grows.** Some cells sprout, the eaten cells lose what was
   eaten, the dead leave a carcass where they fell, and every cell is clamped
   to its bounds.

Three species, one module each, and each imports `life.schema` and the moves
in `life.instinct` and nothing else:

| Species | What it does |
| --- | --- |
| `forager/` | Eats what grows. Runs from a predator, into a thicket when there is one beside it. |
| `predator/` | Hunts what it can reach and follows what it cannot. Sees one cell further than anything else, will not lunge into a thicket, and falls back to grass when it is low. |
| `scavenger/` | Hides where it stands when a predator is near, smells food it cannot see, and waits — until it is hungry, when it goes looking. |

`life.schema` is the whole contract: `SelfView`, `Observation`, `Sighting`,
`Patch`, `Decision`, and `ActionResult`. `life.instinct` is the moves any
creature could make — the open cells, the nearest threat, the way towards
something — written once so that two species cannot break the same tie two
ways. Neither lets a species name `life.world.World`, so a behaviour cannot
read the world it is not being shown and cannot write the world at all. That
is not a sandbox the runtime enforces; it is the module boundary plus the
fact that Cove copies a value into a call.

## What makes it deterministic

Four things, and each is visible in the source rather than promised in prose.

**Chance is a value.** `life.rng` is a linear congruential generator whose
`roll(seed, bound)` answers a `Roll` — the number *and the generator to draw
from next*. A function that draws has to thread the new seed onward, so a
draw shows up in the types. `examples/cq/sample.cove` has the same recurrence,
for the same reason.

**Chance belongs to the world.** `World.seed` is the only generator there is,
and a behaviour is never handed one. Two creatures with the same view answer
the same thing, always, which makes a species a function rather than a
process — the reason `decide` can be tested at all.

**Order is creature id.** Two creatures cannot have one cell, and the earlier
id wins. Id order is the one order a world rebuilt from its seed always has,
so nothing about a run depends on how the intents were collected — and a list
of intents shuffled by its caller is refused rather than applied to the wrong
creature.

**Nothing is asked of the host.** `process.args` once at the start and
`console.println` per reported line — no clock, no environment, and no
filesystem unless `--journal` names one. There is nothing for a host to
answer differently.

The check on all of it is `hash(world)`, a `fold` over the grid and the
population. `world_test.cove` runs the same seed twice and compares the hash
at *every* tick rather than at the end, and the differential harness runs the
whole entry on both backends and compares every line printed. Two runs of
`--seed 42 --ticks 10000` print the same bytes.

## What a copy means here

**The world is a value.** `World` holds `Int`s and `Array`s and no `Vector`,
so `let earlier = world` is a snapshot — not a handle to the same population,
not a deep copy anybody had to write, and not a `Snapshot` conformance. Eight
ticks later, `earlier` is still the world it was. That is one `let`, and it is
the reason a state hash, a journal, and a replay are all easy here.

**A vector is a handle.** The resolution loop keeps its claims in a `Vector`
and hands it to `has(numbers, value)` as an ordinary parameter. The copy that
arrives is an alias, so the helper reads what the loop has pushed by the time
it asks. The tick would put two creatures on one cell if that were not true.

Both halves are pinned in `world/world_test.cove`, and the second one is where
`is` earns its place: two vectors holding equal elements are `==` without
being the same storage, and only `is` can tell you which you have.

The rule this leaves is the one worth taking away: **the shape of the state
decides what a copy means, so choose the shape by what you want a copy to
do.** A world with a `Vector<Creature>` in it would have been a world where
`let earlier = world` quietly recorded nothing.

## Where the higher-order operations earn their place

The two phases of a tick are different shapes because they are different
problems, and the collection API says so:

- **`map` is the decision phase.** No creature's answer can depend on
  another's, because none of them has been carried out yet, so
  `world.creatures.map(...)` is exactly what the phase is.
- **A loop is the resolution phase.** Conflicts are decided by who is first,
  which is a fold with a lot of state and five `Vector`s of scratch — written
  as a loop, because writing it as a `fold` would only have hidden that.
- **`filter` is who survives**, and who a predator can see, and which sighting
  is a threat.
- **`fold` is the hash and the census**, which are the two questions that are
  about the whole world at once. It is also `instinct.nearest`: one answer is
  wanted, and a sort would build a whole ordering to take the front of it.
- **`sorted(by:)` is the population after a birth**, and the deltas before the
  grid is rebuilt, and the sightings a creature is shown. Each of the three is
  a place where "the order two things are in" is part of the answer rather
  than an accident, and each `by` is a strict less-than with an explicit
  tie-break, because a stable sort under a comparison that says nothing about
  ties is a coin toss with extra steps.

## Isolation: what a refusal is for

Issue #91 asks that a broken creature not be able to stop the others. In an
embedding that is a fuel limit; here it is the world's own rule, and it is
checkable in Cove:

```cove
let turn = resolve(world, [
  Intent(id: 1, decision: Decision.Hunt(9999)),
  Intent(id: 2, decision: Decision.Move(Heading.West)),
])
```

Creature 1 hunts something that does not exist. It is `Refused`, it loses its
tick, the refusal is counted in `world.refusals`, and creature 2 moves exactly
as it would have. Nothing is raised, nothing is retried, and the tick
completes. `world.resolve` takes its intents as an argument rather than asking
for them precisely so a test can hand it intents no species would produce.

The scavenger is where this shows up in a real run. Cornered by a predator,
with no thicket beside it and nowhere left to step, it hides where it stands
— and a cell that is not a thicket is not one to hide in, so the world
refuses it. The one-word fix, answering `Rest` in that last case instead, is
deliberately not applied: a species that is sometimes wrong about the world
is what the rule is there for, and a run with no refusals in it would be a
run that proves nothing about them.

## Bounded state, and long runs

The population is capped per species and per cell, and the grid never changes
size, so what a tick costs at the ten-thousandth is what it cost at the tenth.
`world_test.cove` checks the state's size rather than the clock, because the
size is the property and the clock is the machine.

Ten thousand ticks, measured — on the VM, in an **unoptimized** build, which
is what the repository's tests run and is several times slower than
`cove build` produces:

```console
$ cove run life --backend vm --stats -- --seed 7 --ticks 10000 --every 2500
...
tick 10000  alive  11  forager  7  predator  0  scavenger  4  food  309  energy   302  hash 306035650
cove-life: 10000 tick(s), 15 birth(s), 12 death(s), 0 refusal(s), hash 306035650
backend: vm lower=5.973906ms validate=1.257522ms execute=58.390550527s instructions=486139975
stats: fuel_spent=513901036 host_calls=8 irreversible_writes=7 elapsed=58.390698213s wait=249.802µs
heap: allocated=170445 allocated_bytes=8181360 collections=2542 freed=0 live_bytes=0 peak_bytes=23323 pause=299.608068ms
```

5.8 ms and 48,600 instructions a tick, eight host calls for the whole run —
one for the arguments and one per line printed — and a peak heap of **23 KB**.
Eight megabytes were allocated over ten thousand ticks and none of it
accumulated: the world at tick 10,000 is the same size as the world at tick
10, because a tick builds a new world and drops the old one, and the
collector took two and a half thousand passes over three hundred milliseconds
in total to keep it that way. That is the growth question answered by
measurement rather than by argument.

What actually happens over a long run, at the default size, is a finding
rather than a design:

| seed | at tick 200 | at tick 1000 |
| --- | --- | --- |
| 1 | 5 foragers, 2 predators | empty |
| 3 | 7 foragers, 4 scavengers | 7 foragers, 4 scavengers |
| 7 | 7 foragers, 4 scavengers | 7 foragers, 4 scavengers |
| 42 | 7 foragers, 4 scavengers | 7 foragers, 4 scavengers |
| 99 | 2 predators, nothing else | empty |

Three of the five settle at the forager and scavenger caps and stay there for
as long as anybody runs them; two are worlds where the predators got ahead of
the foragers early, ate them, and then starved. Which of the two a seed is
seems to be decided in the first fifty ticks, and nothing after that changes
its mind.

The predators go extinct in every seed that lasts, and a world twice the area
(`--width 16 --height 10`) keeps them for several hundred ticks more. All of
that is the model's answer rather than the runtime's, and tuning the model
until it looked livelier would have said nothing about Cove — which is what
this example is for. What matters here is that the answer is the *same*
answer every time, on both backends.

## What the language made easy, and what it did not

**Easy.** The tick is a pure function from `World` to `World` and nothing had
to be written to make it one: value semantics did it. Exhaustive `match` over
`Species` and `Decision` means adding either is a compile error at every place
that has to change, which is how the `Missed` result and the third species got
added without a search. `test fn` beside the code it tests made the model's
rules — the tie-break, the refusal, the caps — pinnable one at a time.
`sorted(by:)`, `fold`, `filter` and `map` covered every walk in the program
except the resolution loop, which is not a walk.

**Not easy: the grid.** A `Vector` can be pushed onto and can never have an
element replaced, so the grid — the one piece of state a simulation updates in
place — cannot be updated in place. Every tick rebuilds all ninety-six cells
to apply about a dozen changes: the deltas are sorted by cell and merged
against the old grid in one pass, which is the best shape available and is
still O(cells) for O(changes) of work. That is
[issue #154](https://github.com/myuon/cove/issues/154).

**Not easy: asking a sequence a question.** This example writes three helpers
that should not have to exist — `has` and `holds` (does this sequence contain
this number), and `take` (the first n) — because `Array` and `Vector` answer
neither, and `map` and `filter` do not offer an index. That is
[issue #155](https://github.com/myuon/cove/issues/155).

**A name, not a gap.** Issue #91 calls this example `cove-life`. A directory
in a package is a module and a module name component must be a Cove
identifier, so `cove-life/` is refused by `cove::package::module_name` — the
directory is `life/` and the run is `[run.life]`.

## What is not here

Issue #91's V0 is a headless simulation *embedded in a host*. This is the Cove
half of it: the model, the species, the schemas, the determinism, and the
bounded state. What is deliberately not here is everything that is about the
embedding rather than about the language — per-creature fuel budgets and
compiled-module caching (`cove_runtime::embed`'s job, not a Cove program's),
`cove trace`-linked per-tick attribution, a `replay` subcommand, and tick-time
percentiles. Each of those measures the runtime, and this example measures
what a Cove program can say.
