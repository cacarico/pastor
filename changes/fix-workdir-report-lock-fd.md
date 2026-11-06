### Fixed

- A workdir report delivery frees the queue as soon as it ends. Before, a
  program the same process started at that moment could keep the queue
  locked for a short while, and a `workdir place|remove --local` run then
  sent nothing and left its report for the next delivery.
