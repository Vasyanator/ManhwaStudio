import logging
import torch
from saicinpainting.training.trainers.default import DefaultInpaintingTrainingModule


def get_training_model_class(kind):
    if kind == 'default':
        return DefaultInpaintingTrainingModule

    raise ValueError(f'Unknown trainer module {kind}')


def make_training_model(config):
    kind = config.training_model.kind
    kwargs = dict(config.training_model)
    kwargs.pop('kind')
    kwargs['use_ddp'] = config.trainer.kwargs.get('accelerator', None) == 'ddp'

    logging.info(f'Make training model {kind}')

    cls = get_training_model_class(kind)
    return cls(config, **kwargs)


def load_checkpoint(train_config, path, map_location='cuda', strict=True):
    model: torch.nn.Module = make_training_model(train_config)
    state = torch.load(path, map_location=map_location, weights_only=False)
    if isinstance(state, dict) and 'state_dict' in state:
        state_dict = state['state_dict']
        checkpoint_state = state
        load_target = model
    elif isinstance(state, dict) and 'model' in state and isinstance(state['model'], dict):
        state_dict = state['model']
        checkpoint_state = None
        load_target = model.generator
    elif isinstance(state, dict) and 'gen_state_dict' in state and isinstance(state['gen_state_dict'], dict):
        state_dict = state['gen_state_dict']
        checkpoint_state = None
        load_target = model.generator
    elif isinstance(state, dict):
        state_dict = state
        checkpoint_state = None
        load_target = model.generator
    else:
        raise TypeError(f'Unsupported checkpoint format at {path}: {type(state)!r}')

    load_target.load_state_dict(state_dict, strict=strict)
    if checkpoint_state is not None:
        model.on_load_checkpoint(checkpoint_state)
    return model
