### The Main Algorithm in `AreaInner<T>`.

When building a broadcasting synchronization primitive, at some point, when all surface-level optimizations are done, the trade-offs become logical ones in the algorithm itself. There are a few requirements and trade-offs that immediately come to mind; we can then build up the final algorithm by thinking about how to solve each while keeping the compromises to a minimum.

Throughout the document, you'll also see things like this:

> Something something atomics!!! **(Pin 1)**

The **(Pin 1)** means we'll get to it later, aka, we'll put a numbered pin in it.

Let's start with a baseline.

The most important guarantee we need to make is that we are **not allowed to Drop a message if any single active reader might still be using the message.** That is the single defining guarantee.

> **So first, how do we efficiently keep track of every reader's current state?**

Without that knowledge, we will be leaking all messages forever, which is not alright.

*Solution:*

Taking inspiration from the LMAX Disruptor, we can assign each message a sequential number. Let's call it the **generation number**.

**Then, stuff every reader's "private" generation number in a cache-padded array.** For each reader, at most times, this value simply becomes like a local variable thanks to cache-locality. In addition, a cleanup thread can iterate through it quickly to find out what is the last generation it has to keep alive, making the best use of CPU pre-fetching!

We can also take another page out of the LMAX Disruptor book here, and borrow the "reserve slots => write to slots => publish slot to readers" pipeline. This becomes `write_gen` and `read_gen` respectively. So the full process of entering, publishing, and reading becomes the following:

1. **For the writer:**<br>
    *Somehow register yourself as a writer!???* **(Pin 4)**
2. Compete to advance `write_gen` (see functions `try_reserve_slots` and `try_reserve_slots_best_effort`) with a CAS.
3. Write messages into the ring buffer at the slots obtained.
4. Publish to `read_gen` with a CAS, spinning if necessary since publishing has to be sequential (aka, if you own slots 10 to 20 and slot 7 to 9 hasn't been published yet, you need to wait) (see `publish_slots`).

<br>

1. **For the reader:**<br>
    *Somehow register yourself as a reader and get your own private generation number.* **(Pin 1)**
2. **Important: How do we avoid the situation where you enter, and a cleanup thread just so happens to think that the message you entered at has been seen by all *existing* readers, and thus cleans it up, not knowing that a new reader has just entered that believes it can read it???** **This relates to the suspension mechanism (Pin 2). We will return to this problem after that. (Pin 3)**
3. We are good. By default, if we never update our own private generation number, all messages are accessible to us. We only update our private generation number when we are certain we won't need those older messages anymore.
4. Continuously updating the private generation number so that the cleanup thread can clean up messages we don't care about takes up CPU time. Can we... suspend ourselves? Say "I guarantee I will not be trying to dereference *any* messages, and am not in any critical code sections. Please feel free to clean up any message as I am currently suspended and away and unable to update my generation number." **(Pin 2)**

<br>

**For the cleanup thread (there can be multiple, and they should not interfere with each other), we will put a pin in it. (Pin 2)**

Now that we have a basic framework for how everything will work, we enter the nitty-gritty stuff.

The first trade-off we need to consider is:

> **Pin 1: `Flexible entry/exit on-demand` *vs* `Fixed readers and writers upon creation`, and `Entry/exit costs`**

This is the biggest trade-off often made, as many traditional high-performance broadcast systems (such as the LMAX Disruptor) eschew this. This is for a good reason, since there's a good chunk of use-cases for broadcast systems where the readers and writers are basically fixed.

However, can't we have our cake and eat it too? Being able to flexibly create readers and writers on demand would open up a vast amount of new use-cases.

For readers, yes, and this is where the `SimpleLPHashMap<CachePadded<AtomicU64>>` comes in. For our use-case the benefits are immediately clear: It has the same array-like performance characteristics (cache-locality paired with pre-fetching benefits when iterating) while simultaneously being cheap to insert, and due to the use of random keys, is easy to prove to be thread-safe. We introduce slight overhead from the empty space and iteration over that empty space, but since cleanup happens separately and less often, and the sheer performance characteristics of simple iteration is very CPU-friendly, this is a pretty good trade-off.

For writers, since there is no registry like with readers, we use an atomic counter, as during cleanup, we need some way to know if all writers are gone.

> **Pin 2: Clean-up troubles**

How do we clean up?

How?

There are many things to clean up here, so let's break it down.

> Old messages

We had a perfectly neat algorithm above. Unfortunately, actually implementing it, there are nuances that make or break it.

Let's say we implement the following perfectly sensible algorithm for cleaning up older messages:

1. **Draft 1 for the cleanup thread (there can be multiple, and they should not interfere with each other):**<br>
    New global atomic called `last_valid_gen`, which is the last generation number that is still valid.
2. Keep a running variable called `min_held_gen`, which is set to `read_gen` initially.
3. Iterate through **all existing keys** in the private generation numbers map. (highlighted part will be important later, just keep it in mind)
4. `min_held_gen = min_held_gen.min(the_current_readers_gen);`
5. After going through the whole map, keep trying to CAS `last_valid_gen` until we succeed. When we succeed, we are then responsible for cleaning up the range `[what_the_previous_last_valid_gen_was_on_the_successful_CAS, new_last_valid_gen=min_held_gen)`.

Sensible, right?

> Problem: A reader is currently working on something else, a long-running task, and is utterly neglecting keeping its private generation number up-to-date because it's in a hot-loop, and now we are stuck unable to clean up a dangerously growing number of old messages even though no one at all is looking at them.

**This is where we need to start thinking about a suspension mechanism.** How can a thread temporarily say, "I'm out, don't wait up", and then come back later safely?

*Solution:*

We first appropriate the MSB of the private generation number for use as the `IS_SUSPENDED` bit. If the MSB is *not* set, then we follow Draft 1 above. However, if the MSB *is* set...

1. **Draft 2 for the cleanup thread (changes are highlighted):**<br>
    New global atomic called `last_valid_gen`, which is the last generation number that is still valid.
2. Keep a running variable called `min_held_gen`, which is set to `read_gen` initially.
3. Iterate through all existing keys in the private generation numbers map.
4. **If MSB is not set, `min_held_gen = min_held_gen.min(the_current_readers_gen);`**<br>
    **If MSB is set, `forced_update = min_held_gen.max(current_private_gen_without_the_suspend_bit)`, and then we do CAS to try to force an update of the private generation number.**
5. After going through the whole map, keep trying to CAS `last_valid_gen` until we succeed. When we succeed, we are then responsible for cleaning up the range `[what_the_previous_last_valid_gen_was_on_the_successful_CAS, new_last_valid_gen=min_held_gen)`.

Notice what we're doing here, when a reader first decides to suspend itself, it will simply flip the MSB on. And importantly in addition to this, we now make it so that when the reader wants to **get back from suspension, they need to do a CAS on their own private generation number as well!**

This creates the following dynamic:

- While the reader is away, any cleanup thread essentially **forces an update** on the reader's private generation number, keeping the lower 63-bits up-to-date on what generations of messages are *definitely dead*.
- When a reader comes back, they *cannot* update their private generation number to return without acknowledging a cleanup thread has reaped a generation number. Eventually, when the reader succeeds in the CAS then, the claim it stakes is always valid.

Nice!

> **Pin 3**

> Problem... : A reader is *just* registering themselves. Let's say they insert themselves at slot 5 of the `SimpleLPHashMap`. It happily inserts into the slot the current `last_valid_gen`, thinking it has staked a claim and that generation will now remain valid. **However**... At the same time, a clean up thread run is happening, and it has already reached **slot 7**. It will soon update `last_valid_gen` with a CAS, and drop a message that the new reader expects is fully available. We have a use-after-free problem.

To solve this one, we make the realization that re-entry from suspension is in effect, *basically* equivalent to first-time entry.

So we make one last modification 

1. **FINAL DRAFT for the cleanup thread (changes are highlighted):**<br>
    New global atomic called `last_valid_gen`, which is the last generation number that is still valid.
2. Keep a running variable called `min_held_gen`, which is set to `read_gen` initially.
3. Iterate through **ALL SLOTS, NO MATTER IF THERE'S A KEY ASSOCIATED WITH SAID SLOT (basically, initialize all slot values as suspended readers suspended at generation 0; then, treat them as suspended slots despite there not being a reader *really* associated with them)** in the private generation numbers map.
4. If MSB is not set, `min_held_gen = min_held_gen.min(the_current_readers_gen);`<br>
    If MSB is set, `forced_update = min_held_gen.max(current_private_gen_without_the_suspend_bit)`, and then we do CAS to try to force an update of the private generation number.
5. After going through the whole map, keep trying to CAS `last_valid_gen` until we succeed. When we succeed, we are then responsible for cleaning up the range `[what_the_previous_last_valid_gen_was_on_the_successful_CAS, new_last_valid_gen=min_held_gen)`.

> **Pin 4: Final boss**

The `Drop` impl. It is the final boss of our endeavor.

Our goal is pretty straightforward, when either the last `AreaReader` or `AreaWriter` drops, we want them to clean up the whole `AreaInner` structure. To achieve this, we can first think about atomic counters. I like to call the following **tournament-style reference counting** for fun:

> We have N atomic reference counters, one for senders, one for receivers, one for steaks, one for pies, etc. What's important is that we have N atomic reference counters.
> 
> Now, Senders only increment/decrement their own counters, Receivers only increment/decrement their own counters, Steaks only increment/decrement their own counters, and so on and so forth; however, we can only free the underlying structure if *all* are zero.
> 
> So, in a drop impl, both senders and receivers and other types will:
> 
> 1) Grab a raw pointer to a *separately-allocated* `AtomicUsize` called `destroy_stages` (in our situation, the address to this atomic is stored within the main `AreaInner`). This atomic starts off at `N`. It is *leaked* and allocated on the heap, and we will free it separately.
> 2) Decrement their own type-specific-counter. IF they see their own type-specific-counter is now zero, then...
> 3) They move forward and try to decrement **`destroy_stages`** by one. IF they are the one to get `destroy_stages` to zero... they are the winner!! They get to be the one to clean up. They drop the original structure. They also free the `destroy_stages` `AtomicUsize` too.

It's like an elimination-style tournament, and is very straightforward to reason about. We can apply this idea perfectly to the writers, but for the readers, we have an issue.

Readers don't exactly have an "atomic reference counter" we can use. Instead, they have an array of private generation numbers. So when do we decrement? For readers, we need a separate mechanism. It seems like we cannot escape reference counting with an atomic, even when we have a whole registry, but we *can* delay the cost to upon-freeing instead of upon-creation. The idea is very much still rooted in the elimination-tournament idea:

> We first introduce two new atomics called `reader_keep_alloc_tickets` and `reader_stage_tickets`. Both starts at zero.
> 1) First, when a reader's Drop impl is called, we immediately bump `reader_keep_alloc_tickets` up by 1.
> 2) *Then*, we remove ourselves from the table/map.
> 3) After we do so, we run through the entire table/map, checking if there are any other active readers (see `has_readers`).
> Let's pause for a moment here. Notice the logical guarantees here:
>       - *If we find one:* Then it's not our job to free, and importantly when that active reader gets dropped as well, then **we couldn't have blocked them from seeing an empty list of private generation numbers, since we have already removed ourselves.**
>       - *If we don't find one:* We take note that we will be attempting to free the entire structure later on.
> 5) No active => by logic, all other readers that still exist must be in their destructors/drop impls as well, *post* Step 2.
> 6) Now, increment `reader_stage_tickets` *only* if we found no other active readers in that list search, meaning we *will be attempting free*.
> 7) We decrement `reader_keep_alloc_tickets` no matter what.
> 8) If we are *not* going to be attempting to free anything, we exit here. If we *are* going to attempt to free the structure... We basically loop here and wait for `reader_keep_alloc_tickets` to drop to zero. What this ensures is that once `reader_keep_alloc_tickets` hits zero, we *know* that any *remaining* reader Drop implementations are competing to drop, ***and*** `reader_stage_tickets` is the *correct count of competitors (plus one for ourselves)*.
> 9) At this point, the non-competing destructors have all dropped because `reader_keep_alloc_tickets` is zero. `reader_keep_alloc_tickets` served as a gate to let the non-freeing destructors run to completion. Now, the final round. We decrement `reader_stage_tickets`. The one that decrements `reader_stage_tickets` to zero gets to be the one to decrement the global `destroy_stages`, and participate in the aforementioned algorithm. The winner/last-one-out will also free `reader_keep_alloc_tickets` and `reader_stage_tickets`.

An important consideration here is that for this design, we must only allow readers to create readers and writers to create writers.

...And... Voila! Putting everything together, we've got a computer-analogous market with stalls and spices and hot desert winds.

### `SimpleLPHashMap<V>` - Used for the reader private generation numbers map.

**`SimpleLPHashMap<V>`** is a simple thread-safe map, with some unique properties for performance.

- There is very little synchronization provided for the **value** part of the map; the **key** part is the focus.
- Upon creation, a capacity is chosen, which allocates a continuous array to contain that amount of "slots" (key-value pairs).
- Each key is an **AtomicU64**. This is non-negotiable for this map.
- The lower 63-bits of the atomic key is the unique ID, while the MSB is the `IN_PROGRESS` flag.
- **The main insertion mechanism requires that the key be flexible!** This is the biggest difference between this map and regular maps. This is to ensure thread-safety. When a new key-value pair is to be inserted, `get_or_insert_concurrent` is called with `insert` and `fold` both set to true.
- `get_or_insert_concurrent` simply gets or inserts slots, not values. Using the provided key, it indexes into the map and sees if the key is taken. If it is not, the slot is returned. If it is, it begins iterating from that index, up to `n` iterations. This iteration process is what gives meaning to the `placement_offset` value; conceptually, each element in this map is keyed not by just the provided key, but by a `(key, placement_offset)`.
- `fold` then "folds" the `placement_offset` into the key, giving `(key + placement_offset, 0)`. This is important since the two keys are not equivalent; inserting a value with key `(key, placement_offset)`, you will not be able to retrieve the slot later via `(key + placement_offset, 0)`.
- Capturing slots is done via one CAS instruction upon the atomic key.
- When a slot is first obtained, the `IN_PROGRESS` flag is set to one.
- `finish_init_at` clears this flag. This is the only synchronization feature provided for the values of the map.
