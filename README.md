# market_square

In a bustling market square, merchants shout openly at everyone near about their goods, hoping to catch some sales; even in such a raucous place, buyers would then parse all of those things they hear, and buy just the things they came there for; although perhaps they might buy some more.

This crate provides synchronization primitives based on this.

`agora` means a "central public space" or a market in greek, and this word has kept its meaning through many languages, so calling these primitives "agoran" primitives might be appropriate.

## Comparison to Existing Messaging Systems

The most basic form of communication between threads is the messaging channel, which is undoubtedly one-to-one.

Then we have the message queue, which is also typically one-to-one. Yes, queues do allow for multiple threads to consume from the queue at once, but the core assumption of the message queue is that out of all the consumers, only *one* of them will process each item on the queue, and that each consumer is interchangeable (the same); thus, still a one-to-one relationship, just with load balancing. These assumptions end up being useful for many scenarios, but nevertheless they do impose these assumptions upon participants.

If we then attempt to soften these restrictions slightly, for example, by allowing for all consumers to see each message, we get the broadcast channel, found in `tokio` and `postage`, but in such a system the roles of producer and consumer are clearly segmented, and one cannot be both without serious work or a great many number of channels. In addition, many more restrictions abound since the primary purpose that these channels were made for is clear from the name: to broadcast from one broadcaster to many listeners.

## The market square

An agoran queue takes this to an extreme. It's core philosophy is grounded in emulating the bustle of the market square, which is perhaps the most all-encompassing specimen of human communication that can be found, where all forms of human communication abound. Adopting such an idea for compute can thus open up many possibilities.

To start, producers push their messages as normal into the queue, just like in the physical world, anyone can speak and cause air to vibrate, thus creating a message and pushing it into our world; this part is the same as any other messaging system. But the consumer behavior is wildly different, and its aim is to emulate how speech is heard in real life.

Each consumer can at any time look through all messages currently "unclaimed" by any consumer, and until messages are claimed they remain in the queue forever. This allows for inspection by multiple consumers at once of the same messages, and keeping them until they are processed. Read-write locks are used to allow for multiple consumers to inspect at once, thus minimizing as much as possible the thundering-herd problem.

Then, the `autodrop` feature completes the original idea of the bustling crowd in a single physical space, where a message is automatically dropped once all consumers that were present at the time the message was pushed are either gone (dropped) or have skipped over the message to move on to the next. This makes sense, since if you were not physically present when someone says something, then naturally you are likely to miss out on that message.

It's easiest to understand this queue in its final form by an analogy.

Imagine if you are a `MessageConsumer`. You walk into a market square where many people are talking (`push`ers into the queue). When you enter the area, you are guaranteed to hear every single message produced since you've entered into that area, which is the hearing range. That message will then stay alive in the minds of you and everyone else who'd heard it, until such a time when you and all other people who have heard that message decides to forget it. This is what the `market_square` provides.